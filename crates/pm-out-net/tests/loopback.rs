//! A node zone playing through a `pm-endpoint` on loopback into a WAV
//! output: the file is the exact concatenation of the sources, the
//! endpoint refuses a second session on a busy output, and volume on an
//! output without a mixer is refused end to end.

use pm_audio::{LocalFiles, PipelineConfig};
use pm_endpoint::Endpoint;
use pm_model::{AudioFormat, Samples, Volume};
use pm_out_net::NetOutput;
use pm_zones::outputs::{read_wav, NullOutput, WavFileOutput};
use pm_zones::{Insert, Output, OutputRegistry, QueueItem, SinkError, Zone, ZoneId};
use std::net::TcpListener;
use std::sync::Arc;

fn write_wav(path: &std::path::Path, frames: usize, seed: i32) -> Vec<i32> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    let mut v = Vec::new();
    for i in 0..frames * 2 {
        let s = ((i as i64 * 7919 + seed as i64 * 104_729) % (1 << 24)) as i32 - (1 << 23);
        v.push(s);
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zone_plays_bit_exact_through_an_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let out_wav = dir.path().join("endpoint.wav");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(WavFileOutput::new(&out_wav)));
    outputs.register(Arc::new(NullOutput::new()));
    let endpoint = Endpoint::new("Test endpoint", outputs);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let ep = endpoint.clone();
        std::thread::spawn(move || ep.serve(listener).unwrap());
    }

    let remotes = NetOutput::connect(addr).unwrap();
    assert_eq!(remotes.len(), 2);
    let wav_out = remotes
        .into_iter()
        .find(|o| o.remote_id().protocol() == "wav")
        .unwrap();
    assert!(wav_out.name().starts_with("Test endpoint · "));
    assert!(wav_out.id().0.starts_with("net:127.0.0.1:"));
    let wav_out: Arc<dyn Output> = Arc::new(wav_out);

    let a = dir.path().join("a.wav");
    let b = dir.path().join("b.wav");
    let mut want = write_wav(&a, 5000, 1);
    want.extend(write_wav(&b, 3000, 2));
    let zone = Zone::new(
        ZoneId::new("net"),
        "net",
        wav_out.clone(),
        Arc::new(LocalFiles::new(PipelineConfig::default())),
    );
    zone.enqueue(
        vec![
            QueueItem::new(a.to_string_lossy()),
            QueueItem::new(b.to_string_lossy()),
        ],
        Insert::End,
    );
    zone.play();
    let st = zone.wait_stopped().await;
    assert_eq!(st.error, None);
    drop(zone);
    // The endpoint closes the WAV when the session ends; give it a moment.
    for _ in 0..100 {
        if read_wav(&out_wav).map(|w| w.frames() * 2).unwrap_or(0) == want.len() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let back = read_wav(&out_wav).unwrap();
    assert_eq!(back.format, AudioFormat::pcm(48_000, 24, 2));
    assert_eq!(back.samples, Samples::PcmI32(want));

    // Busy: a second session on the same output is refused while one is open.
    let fmt = AudioFormat::pcm(48_000, 24, 2);
    let first = wav_out.open(fmt).await.unwrap();
    let err = match wav_out.open(fmt).await {
        Ok(_) => panic!("second open on a busy output succeeded"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("in use"), "{err}");
    drop(first);

    // Volume on a WAV output: no mixer, refused by caps before any network call.
    assert!(matches!(
        wav_out.set_volume(Volume::Db(-6.0)).await,
        Err(SinkError::NoVolume(_))
    ));
    // An unsupported format is refused locally by caps too.
    assert!(matches!(
        wav_out.open(AudioFormat::dsd(AudioFormat::DSD64_RATE, 2)).await,
        Err(SinkError::UnsupportedFormat(..))
    ));
}
