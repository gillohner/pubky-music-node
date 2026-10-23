//! The crash report, read without adb (STEWARD-FIX-android-crash.md): a Rust panic is written to
//! `crash-report.txt` in the app's files (on Android the same file the Kotlin side,
//! tools/release/android/CrashReport.kt, writes uncaught Java exceptions and the system's last
//! exit reason to). The connect page shows it on the next launch with Copy and Share, and does
//! not reconnect until it is dismissed, so a crash on reconnecting cannot loop. Nothing is sent.

use std::path::PathBuf;
use std::sync::OnceLock;

pub const FILE: &str = "crash-report.txt";

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Android: the app's files folder, known before Tauri is up (`/data/data/<package>/files`).
#[cfg(target_os = "android")]
fn early_dir() -> Option<PathBuf> {
    let cmd = std::fs::read("/proc/self/cmdline").ok()?;
    let pkg = String::from_utf8_lossy(cmd.split(|b| *b == 0).next()?).to_string();
    let pkg = pkg.split(':').next()?.to_string();
    (!pkg.is_empty() && !pkg.contains('/')).then(|| PathBuf::from(format!("/data/data/{pkg}/files")))
}

/// Where reports go: on Android from the start; elsewhere once the app knows its folder.
pub fn set_dir(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = DIR.set(dir);
}

fn path() -> Option<PathBuf> {
    DIR.get().map(|d| d.join(FILE))
}

/// The panic hook: the message and a backtrace into the report, then the default hook.
pub fn install_hook() {
    #[cfg(target_os = "android")]
    if let Some(d) = early_dir() {
        set_dir(d);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let bt = std::backtrace::Backtrace::force_capture();
        let text = format!(
            "== Rust panic on thread \"{thread}\": {info}\nPubky Music {} ({} {})\n{bt}\n\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        tracing::error!("{text}");
        if let Some(p) = path() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = f.write_all(text.as_bytes());
            }
        }
        previous(info);
    }));
}

/// The most of the report the page shows.
const MAX: usize = 64_000;

/// The last report, if there is one: its newest entries, each from its title (`== …`), up to
/// [`MAX`] bytes; an entry longer than that keeps its beginning.
pub fn read() -> Option<String> {
    let t = std::fs::read_to_string(path()?).ok()?;
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    Some(trim_entries(t, MAX))
}

fn trim_entries(t: &str, max: usize) -> String {
    let starts: Vec<usize> = t
        .match_indices("== ")
        .filter(|(i, _)| *i == 0 || t.as_bytes()[i - 1] == b'\n')
        .map(|(i, _)| i)
        .collect();
    if t.len() <= max || starts.is_empty() {
        return cut(t, max).to_string();
    }
    // From the newest entry back, while they fit
    let mut from = *starts.last().expect("one");
    for &s in starts.iter().rev().skip(1) {
        if t.len() - s > max {
            break;
        }
        from = s;
    }
    cut(&t[from..], max).to_string()
}

/// At most `max` bytes, on a character boundary.
fn cut(t: &str, max: usize) -> &str {
    if t.len() <= max {
        return t;
    }
    let mut end = max;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    &t[..end]
}

pub fn clear() {
    if let Some(p) = path() {
        let _ = std::fs::remove_file(p);
    }
}

/// Runs `f`, turning a panic into a logged (and reported) failure: for the startup steps that run
/// on Android's main thread, where an unwinding panic would abort the app.
pub fn guarded<T>(what: &str, f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::error!(
                "{what} failed (a panic, written to the crash report); the app carries on without it"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_lands_in_the_report_and_guarded_carries_on() {
        let dir = std::env::temp_dir().join(format!("pm-crash-{}", std::process::id()));
        set_dir(dir.clone());
        install_hook();
        let r = guarded("a test step", || panic!("boom on purpose"));
        assert!(r.is_none());
        let text = read().expect("a report");
        assert!(text.contains("boom on purpose"), "{text}");
        clear();
        assert!(read().is_none());
        // A long report keeps whole newest entries, from their titles
        let long = format!("== old: a\n{}\n== new: b\n{}", "x".repeat(100), "y".repeat(50));
        assert!(trim_entries(&long, 80).starts_with("== new: b"));
        let huge = format!("== one: c\n{}", "z".repeat(200));
        assert!(trim_entries(&huge, 50).starts_with("== one: c"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
