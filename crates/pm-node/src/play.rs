//! `pm-node info` and `pm-node play`: the M1 local-playback path on the CLI.

use anyhow::Context;
use pm_audio::{probe_file, LocalFiles, PipelineConfig};
use pm_model::SignalPath;
use pm_zones::{
    outputs::WavFileOutput, Insert, Output, OutputId, OutputRegistry, QueueItem, Zone, ZoneEvent, ZoneId,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn info(path: &Path) -> anyhow::Result<()> {
    let fi = probe_file(path).with_context(|| format!("probing {}", path.display()))?;
    println!("file:      {}", path.display());
    println!("container: {}", fi.container);
    println!("format:    {}", fi.format);
    match fi.duration_ms {
        Some(ms) => println!("duration:  {:.3} s", ms as f64 / 1000.0),
        None => println!("duration:  unknown"),
    }
    Ok(())
}

/// Resolve `--to`: an id from the registry (`null`, `alsa:hw:0,0`) or
/// `wav:<path>` for a file output.
pub fn resolve_output(registry: &OutputRegistry, to: &str) -> anyhow::Result<Arc<dyn Output>> {
    if let Some(path) = to.strip_prefix("wav:") {
        return Ok(Arc::new(WavFileOutput::new(path)));
    }
    if let Some(out) = registry.get(&OutputId(to.to_string())) {
        return Ok(out);
    }
    // Any `hw:` name, e.g. `alsa:hw:CARD=KA17,DEV=0` or `alsa:hw:Loopback,0,0`: stable across
    // card renumbering, and reaches subdevices the index-based ids do not list.
    #[cfg(feature = "alsa")]
    if let Some(device) = to.strip_prefix("alsa:") {
        let out = pm_out_local::alsa::AlsaOutput::probe(device, device)
            .with_context(|| format!("probing ALSA device {device}"))?;
        return Ok(Arc::new(out));
    }
    anyhow::bail!("unknown output {to}; run `pm-node outputs`")
}

/// Play `paths` in order through a zone on `output`, printing each track's
/// signal path as it starts. Tracks with the same output format join gapless.
pub async fn play(paths: &[PathBuf], output: Arc<dyn Output>, cfg: PipelineConfig) -> anyhow::Result<()> {
    let zone = Zone::new(
        ZoneId::new("cli"),
        "CLI",
        output.clone(),
        Arc::new(LocalFiles::new(cfg)),
    );
    let mut events = zone.subscribe();
    let items = paths
        .iter()
        .map(|p| QueueItem::new(p.to_string_lossy()))
        .collect();
    zone.enqueue(items, Insert::End);
    zone.play();
    loop {
        match events.recv().await {
            Ok(ZoneEvent::TrackStarted {
                item,
                source_format,
                output_format,
                signal_path,
                gapless,
                ..
            }) => {
                println!(
                    "playing {} : {} -> {}{}",
                    item.uri,
                    source_format,
                    output_format,
                    if gapless { "  (gapless join)" } else { "" }
                );
                print_signal_path(&signal_path);
            }
            Ok(ZoneEvent::TrackEnded { item, verify_ok }) => match verify_ok {
                Some(true) => println!("finished {} : decoder verification ok (MD5 matches)", item.uri),
                Some(false) => println!("finished {} : decoder verification FAILED", item.uri),
                None => println!("finished {}", item.uri),
            },
            Ok(ZoneEvent::Skipped { item, reason }) => println!("skipped {} : {reason}", item.uri),
            Ok(ZoneEvent::Error(e)) => eprintln!("error: {e}"),
            Ok(ZoneEvent::Stopped) => break,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    let st = zone.wait_stopped().await;
    if let Some(e) = st.error {
        anyhow::bail!("{e}");
    }
    Ok(())
}

pub fn print_signal_path(p: &SignalPath) {
    println!("signal path ({:?}):", p.headline());
    for s in &p.stages {
        let note = s.note.as_deref().map(|n| format!("  [{n}]")).unwrap_or_default();
        println!(
            "  {:<12} {} -> {}  {:?}{}",
            s.name, s.in_format, s.out_format, s.quality, note
        );
    }
}
