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
    /// No token: the API listens on loopback only.
    None,
}

/// The token the API requires, and where it came from.
pub fn resolve(configured: &str, listen: &str, data_dir: &Path) -> std::io::Result<(Option<String>, Source)> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return Ok((Some(configured.to_string()), Source::Configured));
    }
    if is_loopback(listen) {
        return Ok((None, Source::None));
    }
    let path = token_path(data_dir);
    Ok((Some(load_or_make(&path)?), Source::File(path)))
}

fn load_or_make(path: &Path) -> std::io::Result<String> {
    if let Some(t) = read(path)? {
        return Ok(t);
    }
    let token = new_token();
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    match o.open(path) {
        Ok(mut f) => {
            f.write_all(token.as_bytes())?;
            f.sync_all()?;
            Ok(token)
        }
        // Another start made it first: use that one. An emptied file stops the start rather
        // than leaving the API open.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => read(path)?.ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidData,
                format!("{} is empty; delete it to make a new token", path.display()),
            )
        }),
        Err(e) => Err(e),
    }
}

fn read(path: &Path) -> std::io::Result<Option<String>> {
    let mut s = String::new();
    match std::fs::File::open(path) {
        Ok(mut f) => {
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
        let (b, _) = resolve("", "[::]:7788", d.path()).unwrap();
        assert_eq!(b.unwrap(), a);
        // An emptied file stops the start rather than leaving the API open
        std::fs::write(token_path(d.path()), "  \n").unwrap();
        assert!(resolve("", "0.0.0.0:7788", d.path()).is_err());
    }
}
