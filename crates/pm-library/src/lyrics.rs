//! Lyrics: embedded tag text or a sidecar `.lrc`, parsed into lines with
//! optional timestamps so a player can follow along.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    /// Position of the line when the text is synchronised (LRC timestamps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    pub text: String,
}

/// Parse LRC or plain text. Lines with `[mm:ss.xx]` tags (several per line
/// allowed) become timed lines sorted by time; metadata tags such as
/// `[ti:…]` are dropped. Returns the lines and whether any was timed.
pub fn parse(text: &str) -> (Vec<LyricLine>, bool) {
    let mut lines = Vec::new();
    let mut synced = false;
    for raw in text.lines() {
        let raw = raw.trim_end();
        let mut rest = raw.trim_start();
        let mut times = Vec::new();
        let mut metadata = false;
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            let tag = &stripped[..end];
            match parse_timestamp(tag) {
                Some(ms) => times.push(ms),
                None if tag.contains(':')
                    && tag
                        .split(':')
                        .next()
                        .is_some_and(|k| k.chars().all(|c| c.is_ascii_alphabetic())) =>
                {
                    metadata = true;
                }
                None => break,
            }
            rest = stripped[end + 1..].trim_start();
        }
        if metadata && times.is_empty() {
            continue;
        }
        let text = rest.trim().to_string();
        if times.is_empty() {
            if !text.is_empty() {
                lines.push(LyricLine { ms: None, text });
            }
        } else {
            synced = true;
            for ms in times {
                lines.push(LyricLine {
                    ms: Some(ms),
                    text: text.clone(),
                });
            }
        }
    }
    if synced {
        lines.retain(|l| l.ms.is_some());
        lines.sort_by_key(|l| l.ms);
    }
    (lines, synced)
}

/// `mm:ss`, `mm:ss.xx`, `mm:ss.xxx` or `hh:mm:ss.xx` → milliseconds.
fn parse_timestamp(tag: &str) -> Option<u64> {
    let mut parts: Vec<&str> = tag.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let last = parts.pop()?;
    let (secs, frac) = match last.split_once('.') {
        Some((s, f)) => (s, f),
        None => (last, ""),
    };
    let secs: u64 = secs.parse().ok()?;
    let frac_ms = match frac.len() {
        0 => 0,
        1 => frac.parse::<u64>().ok()? * 100,
        2 => frac.parse::<u64>().ok()? * 10,
        _ => frac[..3].parse::<u64>().ok()?,
    };
    let mut ms = secs * 1000 + frac_ms;
    let mins: u64 = parts.pop()?.parse().ok()?;
    ms += mins * 60_000;
    if let Some(h) = parts.pop() {
        ms += h.parse::<u64>().ok()? * 3_600_000;
    }
    Some(ms)
}

/// Modification time (unix seconds) of the sidecar `.lrc`, if there is one.
pub fn sidecar_mtime(audio: &Path) -> Option<i64> {
    std::fs::metadata(audio.with_extension("lrc"))
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// The `.lrc` file next to `audio` (same stem), if there is one.
pub fn sidecar(audio: &Path) -> Option<String> {
    let lrc = audio.with_extension("lrc");
    let text = std::fs::read(&lrc).ok()?;
    let text = String::from_utf8_lossy(&text).into_owned();
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc_with_metadata_and_repeats() {
        let (lines, synced) = parse("[ti:Song]\n[ar:Band]\n[00:12.00]Hello\n[01:02.5][00:30.250]Twice\n\n");
        assert!(synced);
        assert_eq!(
            lines
                .iter()
                .map(|l| (l.ms.unwrap(), l.text.as_str()))
                .collect::<Vec<_>>(),
            [(12_000, "Hello"), (30_250, "Twice"), (62_500, "Twice")]
        );
    }

    #[test]
    fn plain_text_keeps_lines() {
        let (lines, synced) = parse("First line\n\n[chorus]\nSecond");
        assert!(!synced);
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["First line", "[chorus]", "Second"]
        );
        assert!(lines.iter().all(|l| l.ms.is_none()));
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp("00:01"), Some(1000));
        assert_eq!(parse_timestamp("01:01.5"), Some(61_500));
        assert_eq!(parse_timestamp("1:00:00.123"), Some(3_600_123));
        assert_eq!(parse_timestamp("ti"), None);
        assert_eq!(parse_timestamp("ab:cd"), None);
    }

    #[test]
    fn sidecar_next_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("a.flac");
        assert!(sidecar(&audio).is_none());
        std::fs::write(dir.path().join("a.lrc"), "[00:00.00]x").unwrap();
        assert_eq!(sidecar(&audio).as_deref(), Some("[00:00.00]x"));
    }
}
