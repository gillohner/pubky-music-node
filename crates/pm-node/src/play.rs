//! `pm-node info` and `pm-node play`: the M1 local-playback path on the CLI.

use anyhow::Context;
use pm_audio::{probe_file, PipelineConfig};
use pm_model::SignalPath;
use pm_zones::{outputs::WavFileOutput, Output, OutputId, OutputRegistry};
use std::path::Path;
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

pub async fn play(path: &Path, output: Arc<dyn Output>, cfg: PipelineConfig) -> anyhow::Result<()> {
    let report = pm_audio::play_file(path, output.as_ref(), &cfg)
        .await
        .with_context(|| format!("playing {} to {}", path.display(), output.id()))?;
    println!(
        "played {} frames: {} -> {}",
        report.frames, report.source_format, report.output_format
    );
    if let Some(ok) = report.verify_ok {
        println!(
            "decoder verification: {}",
            if ok { "ok (MD5 matches)" } else { "FAILED" }
        );
    }
    print_signal_path(&report.signal_path);
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
