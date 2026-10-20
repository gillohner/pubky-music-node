//! Cross-platform local output through `cpal` (CoreAudio on macOS, WASAPI on
//! Windows, ALSA elsewhere), for the native controller on desktops without
//! an `hw:` device of ours. It runs in the operating system's shared mode:
//! the OS mixer may resample to the device rate and mixes in float, so the
//! capability ceiling is "lossless up to the device rate", never bit-perfect.
//! Samples are handed over as 32-bit float, which is lossless for the 16- and
//! 24-bit PCM the pipeline produces.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use pm_model::{AudioBlock, AudioFormat, OutputCaps, QualityCeiling, SampleKind, Samples, Volume};
use pm_zones::{AudioSink, BufferLevel, Output, OutputId, SinkError};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const STANDARD_RATES: [u32; 9] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 768_000,
];
/// How much audio the sink keeps queued for the callback.
const QUEUE_SECONDS: f64 = 0.5;

pub struct CpalOutput {
    device_name: String,
    caps: OutputCaps,
}

impl CpalOutput {
    /// The system's default output device, if any.
    pub fn default_device() -> Option<Self> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        Self::from_device(&device)
    }

    /// Every output device the host lists.
    pub fn enumerate() -> Vec<Self> {
        let host = cpal::default_host();
        host.output_devices()
            .map(|it| it.filter_map(|d| Self::from_device(&d)).collect())
            .unwrap_or_default()
    }

    fn from_device(device: &cpal::Device) -> Option<Self> {
        let device_name = device.name().ok()?;
        let configs: Vec<cpal::SupportedStreamConfigRange> =
            device.supported_output_configs().ok()?.collect();
        if configs.is_empty() {
            return None;
        }
        let mut pcm_rates: Vec<u32> = STANDARD_RATES
            .into_iter()
            .filter(|r| {
                configs
                    .iter()
                    .any(|c| c.min_sample_rate().0 <= *r && *r <= c.max_sample_rate().0 && c.channels() >= 2)
            })
            .collect();
        if pcm_rates.is_empty() {
            pcm_rates = vec![device
                .default_output_config()
                .map(|c| c.sample_rate().0)
                .unwrap_or(48_000)];
        }
        let max_rate = pcm_rates.iter().copied().max().unwrap_or(48_000);
        let max_channels = configs.iter().map(|c| c.channels()).max().unwrap_or(2).min(8) as u8;
        Some(Self {
            device_name,
            caps: OutputCaps {
                pcm_rates,
                max_pcm_bits: 24,
                pcm_bits: vec![16, 24],
                max_channels,
                dsd: None,
                hw_volume: None,
                standby: false,
                source_switch: false,
                gapless: true,
                groupable: false,
                fixed_latency_ms: None,
                quality_ceiling: QualityCeiling::LosslessUpTo {
                    rate_hz: max_rate,
                    bits: 24,
                },
            },
        })
    }
}

fn host_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "CoreAudio"
    } else if cfg!(target_os = "windows") {
        "WASAPI"
    } else {
        "system audio"
    }
}

#[async_trait::async_trait]
impl Output for CpalOutput {
    fn id(&self) -> OutputId {
        OutputId::new("cpal", &self.device_name)
    }

    fn name(&self) -> String {
        format!("{} ({}, shared mode)", self.device_name, host_label())
    }

    fn caps(&self) -> &OutputCaps {
        &self.caps
    }

    async fn open(&self, fmt: AudioFormat) -> Result<Box<dyn AudioSink>, SinkError> {
        if fmt.kind != SampleKind::Pcm || !self.caps.pcm_rates.contains(&fmt.rate_hz) || fmt.channels < 1 {
            return Err(SinkError::UnsupportedFormat(self.id(), fmt));
        }
        CpalSink::open(&self.device_name, self.id(), fmt).map(|s| Box::new(s) as Box<dyn AudioSink>)
    }

    async fn set_volume(&self, _v: Volume) -> Result<(), SinkError> {
        Err(SinkError::NoVolume(self.id()))
    }
}

struct Shared {
    queue: Mutex<VecDeque<f32>>,
    /// Frames the callback has consumed (what the user hears, roughly).
    played: AtomicU64,
    underruns: AtomicU64,
    stop: AtomicBool,
}

/// The `cpal::Stream` is not `Send` on every platform, so it lives on its own
/// thread; the sink talks to it through the shared queue.
pub struct CpalSink {
    id: OutputId,
    fmt: AudioFormat,
    shared: Arc<Shared>,
    capacity_frames: u64,
    written: u64,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CpalSink {
    fn open(device_name: &str, id: OutputId, fmt: AudioFormat) -> Result<Self, SinkError> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            played: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        let capacity_frames = (fmt.rate_hz as f64 * QUEUE_SECONDS) as u64;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let name = device_name.to_string();
        let channels = fmt.channels as usize;
        let s2 = shared.clone();
        let thread = std::thread::Builder::new()
            .name("cpal-output".into())
            .spawn(move || {
                let host = cpal::default_host();
                let device = host
                    .output_devices()
                    .ok()
                    .and_then(|mut it| it.find(|d| d.name().map(|n| n == name).unwrap_or(false)));
                let Some(device) = device else {
                    let _ = ready_tx.send(Err(format!("output device {name} is gone")));
                    return;
                };
                let config = cpal::StreamConfig {
                    channels: channels as u16,
                    sample_rate: cpal::SampleRate(fmt.rate_hz),
                    buffer_size: cpal::BufferSize::Default,
                };
                let s3 = s2.clone();
                let stream = device.build_output_stream(
                    &config,
                    move |out: &mut [f32], _| {
                        let mut q = s3.queue.lock().unwrap_or_else(|p| p.into_inner());
                        let have = q.len().min(out.len());
                        for (i, v) in out.iter_mut().enumerate() {
                            *v = if i < have {
                                q.pop_front().unwrap_or(0.0)
                            } else {
                                0.0
                            };
                        }
                        if have < out.len() && have > 0 {
                            s3.underruns.fetch_add(1, Ordering::Relaxed);
                        }
                        s3.played
                            .fetch_add((have / channels.max(1)) as u64, Ordering::Relaxed);
                    },
                    |e| tracing::warn!("cpal stream error: {e}"),
                    None,
                );
                let stream = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                if let Err(e) = stream.play() {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                while !s2.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                drop(stream);
            })
            .map_err(|e| SinkError::Device(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(SinkError::Unavailable(id, e)),
            Err(_) => return Err(SinkError::Unavailable(id, "audio thread ended".into())),
        }
        Ok(Self {
            id,
            fmt,
            shared,
            capacity_frames,
            written: 0,
            thread: Some(thread),
        })
    }

    fn queued_frames(&self) -> u64 {
        let q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        (q.len() / self.fmt.channels.max(1) as usize) as u64
    }

    /// Wait until the queue has room for `frames` more.
    fn wait_for_room(&self, frames: u64) {
        while self.queued_frames() + frames > self.capacity_frames * 2 {
            if self.shared.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

impl AudioSink for CpalSink {
    fn format(&self) -> AudioFormat {
        self.fmt
    }

    fn write(&mut self, block: &AudioBlock) -> Result<(), SinkError> {
        if block.format != self.fmt {
            return Err(SinkError::FormatMismatch {
                expected: self.fmt,
                found: block.format,
            });
        }
        let scale = 1.0 / (1u64 << (self.fmt.bits.saturating_sub(1).min(31))) as f32;
        let samples: Vec<f32> = match &block.samples {
            Samples::PcmI32(s) => s.iter().map(|&v| v as f32 * scale).collect(),
            Samples::PcmF64(s) => s.iter().map(|&v| v as f32).collect(),
            Samples::Dsd(_) => return Err(SinkError::UnsupportedFormat(self.id.clone(), block.format)),
        };
        self.wait_for_room(block.frames() as u64);
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend(samples);
        self.written += block.frames() as u64;
        Ok(())
    }

    fn buffer_level(&self) -> BufferLevel {
        BufferLevel {
            queued_frames: self.queued_frames(),
            capacity_frames: self.capacity_frames * 2,
        }
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        Ok(())
    }

    fn drain(&mut self) -> Result<(), SinkError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.queued_frames() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Let the device play out what the OS still buffers.
        std::thread::sleep(std::time::Duration::from_millis(50));
        Ok(())
    }

    fn frames_written(&self) -> u64 {
        self.written
    }
}

impl Drop for CpalSink {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let underruns = self.shared.underruns.load(Ordering::Relaxed);
        if underruns > 0 {
            tracing::debug!(output = %self.id, underruns, "cpal sink closed");
        }
    }
}
