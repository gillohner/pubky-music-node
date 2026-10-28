//! The API token. An admin API open to the network is never the default: when `listen` is
//! not a loopback address and no token is configured, the node makes one on first start and
//! keeps it in `<data_dir>/api-token` (0600), the same one on every later start.

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

/// Where a token the node made for itself is kept.
pub fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join("api-token")
}

/// Whether `listen` (`host:port`) only takes connections from this machine. A name other than
/// `localhost`, or anything that does not parse, counts as reachable from the network.
pub fn is_loopback(listen: &str) -> bool {
    if let Ok(a) = listen.parse::<SocketAddr>() {
        return a.ip().is_loopback();
    }
    if let Ok(ip) = listen.parse::<IpAddr>() {
        return ip.is_loopback();
    }
    listen
        .rsplit_once(':')
        .map_or(listen, |(h, _)| h)
        .eq_ignore_ascii_case("localhost")
}

/// What the token comes from, for the start-up log line (never the token itself).
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    Configured,
    /// Made now or on an earlier start; kept in this file.
    File(PathBuf),
    /// The configured token was a placeholder from the old docs, so the one in this file is used.
    ReplacedPlaceholder(PathBuf),
    /// No token: the API listens on loopback only.
    None,
}

/// Tokens the old docs and the TrueNAS file handed out: anyone who read them knows these.
const PLACEHOLDERS: &[&str] = &["change-me", "changeme", "change-me-to-a-long-random-secret"];

/// The token the API requires, and where it came from.
pub fn resolve(configured: &str, listen: &str, data_dir: &Path) -> std::io::Result<(Option<String>, Source)> {
    let configured = configured.trim();
    let placeholder = PLACEHOLDERS.iter().any(|p| configured.eq_ignore_ascii_case(p));
    if !configured.is_empty() && !placeholder {
        if configured.len() < 16 {
            tracing::warn!("[api] token is shorter than 16 characters; a long random one is safer");
        }
        return Ok((Some(configured.to_string()), Source::Configured));
    }
    if !placeholder && is_loopback(listen) {
        return Ok((None, Source::None));
    }
    let path = token_path(data_dir);
    let token =
        load_or_make(&path).map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    Ok((
        Some(token),
        if placeholder {
            Source::ReplacedPlaceholder(path)
        } else {
            Source::File(path)
        },
    ))
}

fn load_or_make(path: &Path) -> std::io::Result<String> {
    if let Some(t) = read(path)? {
        return Ok(t);
    }
    // Written whole to a side file, then linked into place (never over an existing one), so a
    // crash or a full disk never leaves an empty token file behind.
    let token = new_token();
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let written = o.open(&tmp).and_then(|mut f| {
        f.write_all(token.as_bytes())?;
        f.sync_all()
    });
    let linked = written.and_then(|()| std::fs::hard_link(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
            }
            Ok(token)
        }
        // Another start made it first: use that one. An emptied file stops the start rather
        // than leaving the API open.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => read(path)?.ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidData,
                "the file is empty; delete it to make a new token",
            )
        }),
        Err(e) => Err(e),
    }
}

fn read(path: &Path) -> std::io::Result<Option<String>> {
    let mut s = String::new();
    match std::fs::File::open(path) {
        Ok(mut f) => {
            #[cfg(unix)]
            {
                // A token written by hand may have come out world-readable
                use std::os::unix::fs::PermissionsExt;
                if f.metadata()?.permissions().mode() & 0o077 != 0 {
                    tracing::warn!(path = %path.display(), "the API token file was readable by others; now 0600");
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                }
            }
            f.read_to_string(&mut s)?;
            let t = s.trim();
            Ok((!t.is_empty()).then(|| t.to_string()))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn new_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_addresses_are_told_apart_from_the_network() {
        for l in [
            "127.0.0.1:7788",
            "127.8.0.1:80",
            "localhost:7788",
            "[::1]:7788",
            "::1",
            "LOCALHOST:1",
        ] {
            assert!(is_loopback(l), "{l}");
        }
        for l in [
            "0.0.0.0:7788",
            "[::]:7788",
            "192.168.1.5:7788",
            "nas.local:7788",
            "127.example.org:7788",
            "localhost.evil:1",
            "",
        ] {
            assert!(!is_loopback(l), "{l}");
        }
    }

    #[test]
    fn a_configured_token_wins_and_loopback_needs_none() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve(" s3cret ", "0.0.0.0:7788", d.path()).unwrap(),
            (Some("s3cret".into()), Source::Configured)
        );
        assert_eq!(
            resolve("", "127.0.0.1:7788", d.path()).unwrap(),
            (None, Source::None)
        );
        assert!(!token_path(d.path()).exists());
    }

    #[test]
    fn a_documented_placeholder_is_never_the_token() {
        let d = tempfile::tempdir().unwrap();
        let (t, src) = resolve("change-me-to-a-long-random-secret", "0.0.0.0:7788", d.path()).unwrap();
        let t = t.unwrap();
        assert_eq!(t.len(), 64);
        assert_eq!(src, Source::ReplacedPlaceholder(token_path(d.path())));
        // Even on loopback, and the same made token every time
        let (u, _) = resolve(" Change-Me ", "127.0.0.1:7788", d.path()).unwrap();
        assert_eq!(u.unwrap(), t);
    }

    #[cfg(unix)]
    #[test]
    fn a_hand_written_token_file_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = token_path(d.path());
        std::fs::write(&p, "a-token-written-by-hand\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (t, _) = resolve("", "0.0.0.0:7788", d.path()).unwrap();
        assert_eq!(t.as_deref(), Some("a-token-written-by-hand"));
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn a_network_listener_without_a_token_gets_one_kept_across_starts() {
        let d = tempfile::tempdir().unwrap();
        let (a, src) = resolve("", "0.0.0.0:7788", d.path()).unwrap();
        let a = a.unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(src, Source::File(token_path(d.path())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(token_path(d.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o600);
        }
        // Written through a side file, none left behind
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
        let (b, _) = resolve("", "[::]:7788", d.path()).unwrap();
        assert_eq!(b.unwrap(), a);
        // An emptied file stops the start rather than leaving the API open
        std::fs::write(token_path(d.path()), "  \n").unwrap();
        assert!(resolve("", "0.0.0.0:7788", d.path()).is_err());
    }
}
