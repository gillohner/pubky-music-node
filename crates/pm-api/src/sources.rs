//! The node's [`SourceFactory`]: queue items are library tracks
//! (`track:<id>`), local paths, `file://` URLs and `http(s)://` streams
//! (internet radio, direct file URLs; M3U/PLS playlists are followed once).
//! Streaming services (M12)
//! plug in here as further URI schemes.

use pm_audio::{LocalFiles, PipelineConfig};
use pm_library::Library;
use pm_model::{AudioBlock, AudioFormat, OutputCaps, TrackId};
use pm_zones::{FrameSource, OutputId, Processor, QueueItem, SourceError, SourceFactory};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub const TRACK_SCHEME: &str = "track:";
/// Settings key prefix for per-output DSP configs.
pub const DSP_KEY_PREFIX: &str = "dsp/";

pub struct NodeSources {
    library: Arc<Mutex<Library>>,
    files: LocalFiles,
    default_cfg: PipelineConfig,
    /// Per-output pipeline settings (DSP); the default applies otherwise.
    dsp: RwLock<HashMap<OutputId, PipelineConfig>>,
    /// TIDAL credentials and origins, for `tidal:track:` URIs on the audio thread.
    tidal: crate::tidal::SharedEndpoints,
    /// Deezer API origin, for `deezer:track:` URIs (previews).
    deezer: crate::deezer::SharedBase,
    /// Jamendo credentials, for `jamendo:track:` URIs.
    jamendo: crate::jamendo::SharedEndpoints,
    /// The user's Subsonic server, for `subsonic:track:` URIs.
    opensubsonic: crate::opensubsonic::Shared,
    /// The user's Jellyfin, for `jellyfin:track:` URIs.
    jellyfin: crate::jellyfin::Shared,
    /// The user's Plex, for `plex:track:` URIs.
    plex: crate::plex::Shared,
    /// The user's Audiobookshelf, for `audiobookshelf:file:` URIs.
    audiobookshelf: crate::audiobookshelf::Shared,
    /// WebDAV shares, for library files on them (`webdav://…`).
    webdav: crate::webdav::Shared,
}

impl NodeSources {
    /// One handle per source that opens its own URIs on the audio thread.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        library: Arc<Mutex<Library>>,
        cfg: PipelineConfig,
        tidal: crate::tidal::SharedEndpoints,
        deezer: crate::deezer::SharedBase,
        jamendo: crate::jamendo::SharedEndpoints,
        opensubsonic: crate::opensubsonic::Shared,
        jellyfin: crate::jellyfin::Shared,
        plex: crate::plex::Shared,
        audiobookshelf: crate::audiobookshelf::Shared,
        webdav: crate::webdav::Shared,
    ) -> Self {
        // Restore persisted per-output configs.
        let mut dsp = HashMap::new();
        if let Ok(lib) = library.lock() {
            if let Ok(rows) = lib.settings_with_prefix(DSP_KEY_PREFIX) {
                for (k, v) in rows {
                    if let Ok(c) = serde_json::from_str::<PipelineConfig>(&v) {
                        dsp.insert(OutputId(k[DSP_KEY_PREFIX.len()..].to_string()), c);
                    }
                }
            }
        }
        Self {
            library,
            files: LocalFiles::new(cfg.clone()),
            default_cfg: cfg,
            dsp: RwLock::new(dsp),
            tidal,
            deezer,
            jamendo,
            opensubsonic,
            jellyfin,
            plex,
            audiobookshelf,
            webdav,
        }
    }

    /// The pipeline settings in force for `output`.
    pub fn dsp_for(&self, output: &OutputId) -> PipelineConfig {
        self.dsp
            .read()
            .expect("dsp lock")
            .get(output)
            .cloned()
            .unwrap_or_else(|| self.default_cfg.clone())
    }

    /// Set (and persist) the settings for `output`; takes effect at the next track start.
    pub fn set_dsp(
        &self,
        output: &OutputId,
        cfg: Option<PipelineConfig>,
    ) -> Result<(), pm_library::LibraryError> {
        let key = format!("{DSP_KEY_PREFIX}{}", output.0);
        let lib = self.library.lock().expect("library poisoned");
        match &cfg {
            Some(c) => lib.set_setting(&key, &serde_json::to_string(c).expect("config serializes"))?,
            None => {
                lib.delete_setting(&key)?;
            }
        }
        drop(lib);
        let mut map = self.dsp.write().expect("dsp lock");
        match cfg {
            Some(c) => {
                map.insert(output.clone(), c);
            }
            None => {
                map.remove(output);
            }
        }
        Ok(())
    }

    /// `track:<id>` for a library track.
    pub fn track_uri(id: TrackId) -> String {
        format!("{TRACK_SCHEME}{}", id.0)
    }

    /// A queue item for a library track, with its display metadata.
    pub fn item_for(track: &pm_model::Track) -> Option<QueueItem> {
        let id = track.id?;
        let mut item = QueueItem::new(Self::track_uri(id));
        item.title = Some(track.title.clone());
        item.artist = track.artists.first().map(|a| a.name.clone());
        item.album = track.album.as_ref().map(|a| a.title.clone());
        item.duration_ms = track.duration_ms;
        Some(item)
    }
}

/// Blocking HTTP client for stream sources, built on first use on the audio
/// thread (never inside a tokio runtime). No overall timeout: a live stream
/// never ends. A stalled server keeps the zone waiting until it stops it.
pub(crate) fn http_client() -> &'static reqwest::blocking::Client {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent(concat!("pubky-music-node/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(10))
            // Dead peers are noticed by keepalive; the body reader thread
            // then ends instead of lingering until the OS gives up.
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .timeout(None)
            .build()
            .expect("http client")
    })
}

/// How long a live stream may go silent before the track ends with an error
/// instead of the zone waiting forever.
const STREAM_STALL: std::time::Duration = std::time::Duration::from_secs(30);

/// Reads the body on its own thread and hands chunks over a bounded channel,
/// so the audio thread's reads can time out (`STREAM_STALL`). The reader
/// thread ends when the receiver is dropped.
struct TimeoutReader {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    pending: Vec<u8>,
    at: usize,
    stall: std::time::Duration,
    done: bool,
}

impl TimeoutReader {
    fn new(mut inner: Box<dyn std::io::Read + Send>, stall: std::time::Duration) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(64);
        std::thread::Builder::new()
            .name("http-stream".into())
            .spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    let msg = match inner.read(&mut buf) {
                        Ok(0) => Ok(Vec::new()),
                        Ok(n) => Ok(buf[..n].to_vec()),
                        Err(e) => Err(e),
                    };
                    let end = matches!(&msg, Ok(v) if v.is_empty()) || msg.is_err();
                    if tx.send(msg).is_err() || end {
                        break;
                    }
                }
            })
            .expect("spawn http-stream thread");
        Self {
            rx,
            pending: Vec::new(),
            at: 0,
            stall,
            done: false,
        }
    }
}

impl std::io::Read for TimeoutReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= self.pending.len() {
            if self.done {
                return Ok(0);
            }
            match self.rx.recv_timeout(self.stall) {
                Ok(Ok(chunk)) if chunk.is_empty() => {
                    self.done = true;
                    return Ok(0);
                }
                Ok(Ok(chunk)) => {
                    self.pending = chunk;
                    self.at = 0;
                }
                Ok(Err(e)) => {
                    self.done = true;
                    return Err(e);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("stream sent nothing for {} s", self.stall.as_secs()),
                    ));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.done = true;
                    return Ok(0);
                }
            }
        }
        let n = buf.len().min(self.pending.len() - self.at);
        buf[..n].copy_from_slice(&self.pending[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// `Read` over the stream body, `Send + Sync` for symphonia.
struct HttpBody(std::sync::Mutex<TimeoutReader>);

impl std::io::Read for HttpBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.get_mut().expect("body lock").read(buf)
    }
}

/// The latest title an ICY stream announced, shared between the byte reader
/// (which sees the metadata) and the frame source (which the zone polls).
type SharedTitle = Arc<Mutex<Option<String>>>;

/// Strips SHOUTcast/Icecast in-band metadata: after every `interval` audio
/// bytes comes one length byte `n` and `16 * n` bytes of `key='value';`
/// pairs. `StreamTitle` is kept, everything else dropped.
struct IcyReader<R: std::io::Read> {
    inner: R,
    interval: usize,
    /// Audio bytes left before the next metadata block.
    remaining: usize,
    title: SharedTitle,
}

impl<R: std::io::Read> IcyReader<R> {
    fn new(inner: R, interval: usize, title: SharedTitle) -> Self {
        Self {
            inner,
            interval,
            remaining: interval,
            title,
        }
    }

    fn read_exact_or_eof(&mut self, buf: &mut [u8]) -> std::io::Result<bool> {
        let mut got = 0;
        while got < buf.len() {
            let n = self.inner.read(&mut buf[got..])?;
            if n == 0 {
                return Ok(false);
            }
            got += n;
        }
        Ok(true)
    }

    fn skip_metadata(&mut self) -> std::io::Result<()> {
        let mut len = [0u8; 1];
        if !self.read_exact_or_eof(&mut len)? {
            return Ok(());
        }
        let n = len[0] as usize * 16;
        if n > 0 {
            let mut meta = vec![0u8; n];
            if self.read_exact_or_eof(&mut meta)? {
                if let Some(t) = parse_icy_title(&meta) {
                    *self.title.lock().expect("title lock") = Some(t);
                }
            }
        }
        self.remaining = self.interval;
        Ok(())
    }
}

impl<R: std::io::Read> std::io::Read for IcyReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            self.skip_metadata()?;
        }
        let want = buf.len().min(self.remaining);
        let n = self.inner.read(&mut buf[..want])?;
        self.remaining -= n;
        Ok(n)
    }
}

/// `StreamTitle='Artist - Title';` out of a metadata block (NUL padded).
pub(crate) fn parse_icy_title(meta: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(meta);
    let text = text.trim_end_matches('\0');
    let start = text.find("StreamTitle='")? + "StreamTitle='".len();
    let rest = &text[start..];
    let end = rest.find("';").unwrap_or(rest.len());
    let title = rest[..end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

/// An HTTP stream as a frame source: the decoded audio plus the ICY titles.
struct HttpStream {
    inner: pm_audio::play::Source,
    title: SharedTitle,
    last: Option<String>,
}

impl FrameSource for HttpStream {
    fn format(&self) -> AudioFormat {
        self.inner.format()
    }
    fn duration_ms(&self) -> Option<u64> {
        self.inner.duration_ms()
    }
    fn next_block(&mut self) -> Result<Option<AudioBlock>, SourceError> {
        self.inner
            .next_block()
            .map_err(|e| SourceError::Decode(e.to_string()))
    }
    fn seek_ms(&mut self, ms: u64) -> Result<(), SourceError> {
        self.inner
            .seek_ms(ms)
            .map_err(|e| SourceError::Seek(e.to_string()))
    }
    fn finalize(&mut self) -> Option<bool> {
        self.inner.finalize()
    }
    fn lossy_source(&self) -> bool {
        self.inner.is_lossy()
    }
    fn stream_title(&mut self) -> Option<String> {
        let now = self.title.lock().expect("title lock").clone();
        if now.is_some() && now != self.last {
            self.last = now.clone();
            return now;
        }
        None
    }
}

/// Container hint from a `Content-Type`.
pub(crate) fn ext_for_mime(mime: &str) -> Option<&'static str> {
    let m = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    Some(match m.as_str() {
        "audio/mpeg" | "audio/mp3" | "audio/mpa" => "mp3",
        "audio/aac" | "audio/aacp" | "audio/x-aac" => "aac",
        "audio/mp4" | "audio/x-m4a" | "video/mp4" => "m4a",
        "audio/ogg" | "application/ogg" | "audio/vorbis" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        _ => return None,
    })
}

/// Container hint from a URL's extension.
pub(crate) fn ext_for_url(url: &str) -> Option<&'static str> {
    let path = url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    let e = path.rsplit('.').next().unwrap_or("");
    Some(match e {
        "mp3" => "mp3",
        "aac" => "aac",
        "m4a" | "mp4" => "m4a",
        "ogg" | "oga" | "opus" => "ogg",
        "flac" => "flac",
        "wav" => "wav",
        _ => return None,
    })
}

/// Is this a playlist file (M3U/PLS) rather than audio?
pub(crate) fn is_playlist(url: &str, mime: &str) -> bool {
    let m = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let path = url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    matches!(
        m.as_str(),
        "audio/x-mpegurl"
            | "application/vnd.apple.mpegurl"
            | "audio/mpegurl"
            | "audio/x-scpls"
            | "application/pls+xml"
    ) || path.ends_with(".m3u")
        || path.ends_with(".m3u8")
        || path.ends_with(".pls")
}

/// The first stream URL in an M3U or PLS body.
pub(crate) fn first_stream_url(body: &str) -> Option<String> {
    for line in body.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let candidate = match l.split_once('=') {
            Some((k, v)) if k.trim().to_ascii_lowercase().starts_with("file") => v.trim(),
            Some(_) if !l.starts_with("http") => continue,
            _ => l,
        };
        if candidate.starts_with("http://") || candidate.starts_with("https://") {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Open an HTTP(S) stream (internet radio, a direct file URL) as a frame
/// source. Playlist URLs (M3U/PLS) are followed once to their first stream.
/// Blocking; call from an audio thread. Shared by the node and the app's
/// local zone.
pub fn open_http_stream(uri: &str) -> Result<Box<dyn FrameSource>, SourceError> {
    {
        let err = |reason: String| SourceError::Open {
            uri: uri.to_string(),
            reason,
        };
        let mut url = uri.to_string();
        for hop in 0..2 {
            let resp = http_client()
                .get(&url)
                .header("Icy-MetaData", "1")
                .send()
                .map_err(|e| err(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(err(format!("HTTP {}", resp.status())));
            }
            let mime = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if is_playlist(&url, &mime) && hop == 0 {
                let text = resp.text().map_err(|e| err(e.to_string()))?;
                url = first_stream_url(&text).ok_or_else(|| err("playlist has no stream URL".into()))?;
                continue;
            }
            let ext = ext_for_mime(&mime).or_else(|| ext_for_url(&url));
            let metaint = resp
                .headers()
                .get("icy-metaint")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|n| *n > 0);
            let title: SharedTitle = Arc::new(Mutex::new(None));
            let body = HttpBody(std::sync::Mutex::new(TimeoutReader::new(
                Box::new(resp),
                STREAM_STALL,
            )));
            let inner = match metaint {
                Some(n) => {
                    let media =
                        symphonia::core::io::ReadOnlySource::new(IcyReader::new(body, n, title.clone()));
                    pm_audio::play::Source::from_media_source(Box::new(media), ext, &url)
                }
                None => {
                    let media = symphonia::core::io::ReadOnlySource::new(body);
                    pm_audio::play::Source::from_media_source(Box::new(media), ext, &url)
                }
            }
            .map_err(|e| err(e.to_string()))?;
            return Ok(Box::new(HttpStream {
                inner,
                title,
                last: None,
            }));
        }
        Err(err("too many playlist redirections".into()))
    }
}

/// `http://` or `https://`.
/// A byte stream (segments, a proxy body…) as a zone source: stall watchdog,
/// symphonia probe by `ext` hint, no in-stream titles.
pub(crate) fn frame_source_from_reader(
    reader: Box<dyn std::io::Read + Send>,
    ext: Option<&str>,
    name: &str,
) -> Result<Box<dyn FrameSource>, SourceError> {
    let body = HttpBody(std::sync::Mutex::new(TimeoutReader::new(reader, STREAM_STALL)));
    let media = symphonia::core::io::ReadOnlySource::new(body);
    let inner = pm_audio::play::Source::from_media_source(Box::new(media), ext, name).map_err(|e| {
        SourceError::Open {
            uri: name.to_string(),
            reason: e.to_string(),
        }
    })?;
    Ok(Box::new(HttpStream {
        inner,
        title: Arc::new(Mutex::new(None)),
        last: None,
    }))
}

pub fn is_http_uri(uri: &str) -> bool {
    uri.starts_with("http://") || uri.starts_with("https://")
}

impl SourceFactory for NodeSources {
    fn open(&self, item: &QueueItem) -> Result<Box<dyn FrameSource>, SourceError> {
        if let Some(id) = item.uri.strip_prefix(crate::youtube::VIDEO_URI_PREFIX) {
            // Through yt-dlp when the owner switched that on (unofficial); else This browser only
            if self.library.lock().is_ok_and(|l| crate::ytdlp::zones_on_lib(&l)) {
                return crate::ytdlp::open_blocking(&self.library, id);
            }
            return Err(SourceError::Open {
                uri: item.uri.clone(),
                reason: crate::youtube::ZONE_REFUSAL.into(),
            });
        }
        if is_http_uri(&item.uri) {
            return open_http_stream(&item.uri);
        }
        if item.uri == crate::librespot::LIVE_URI {
            return crate::librespot::open_live();
        }
        if let Some(id) = item.uri.strip_prefix(crate::tidal::TRACK_URI_PREFIX) {
            return crate::tidal::open_track_blocking(&self.library, &self.tidal, id);
        }
        if let Some(id) = item.uri.strip_prefix(crate::deezer::TRACK_URI_PREFIX) {
            return crate::deezer::open_track_blocking(&self.deezer, id);
        }
        if let Some(id) = item.uri.strip_prefix(crate::qobuz::TRACK_URI_PREFIX) {
            return crate::qobuz::open_track_blocking(&self.library, id);
        }
        if let Some(id) = item.uri.strip_prefix(crate::jamendo::TRACK_URI_PREFIX) {
            return crate::jamendo::open_track_blocking(&self.jamendo, id);
        }
        if let Some(id) = item.uri.strip_prefix(crate::opensubsonic::TRACK_URI_PREFIX) {
            return crate::opensubsonic::open_track_blocking(&self.opensubsonic, id);
        }
        if let Some(id) = item.uri.strip_prefix(crate::jellyfin::TRACK_URI_PREFIX) {
            return crate::jellyfin::open_track_blocking(&self.jellyfin, id);
        }
        if let Some(id) = item.uri.strip_prefix(crate::plex::TRACK_URI_PREFIX) {
            return crate::plex::open_track_blocking(&self.plex, id);
        }
        if let Some(rest) = item.uri.strip_prefix(crate::audiobookshelf::TRACK_URI_PREFIX) {
            return crate::audiobookshelf::open_track_blocking(&self.audiobookshelf, rest);
        }
        let Some(id) = item.uri.strip_prefix(TRACK_SCHEME) else {
            return self.files.open(item);
        };
        let id: i64 = id.parse().map_err(|_| SourceError::Open {
            uri: item.uri.clone(),
            reason: "malformed track id".into(),
        })?;
        let track = self
            .library
            .lock()
            .expect("library poisoned")
            .get_track(TrackId(id))
            .map_err(|e| SourceError::Open {
                uri: item.uri.clone(),
                reason: e.to_string(),
            })?
            .ok_or_else(|| SourceError::Open {
                uri: item.uri.clone(),
                reason: "no such track".into(),
            })?;
        let inner = if crate::webdav::is_key(&track.file.path) {
            crate::webdav::open_blocking(&self.webdav, &track.file.path)?
        } else {
            let mut file_item = QueueItem::new(track.file.path.to_string_lossy());
            file_item.id = item.id;
            self.files.open(&file_item)?
        };
        Ok(Box::new(pm_zones::WithReplayGain {
            inner,
            gain: pm_zones::ReplayGain {
                track_db: track.replaygain_track_gain_db,
                album_db: track.replaygain_album_gain_db,
            },
        }))
    }

    fn plan(
        &self,
        src: &dyn FrameSource,
        caps: &OutputCaps,
        output: &OutputId,
    ) -> Result<Box<dyn Processor>, SourceError> {
        let source = src.format();
        let cfg = self.dsp_for(output);
        let mut plan = pm_audio::Plan::negotiate_for(source, caps, &cfg, src.replay_gain()).map_err(|e| {
            SourceError::Negotiate {
                format: source,
                reason: e.to_string(),
            }
        })?;
        if src.lossy_source() {
            plan.mark_lossy_source();
        }
        if output.0.starts_with("group:") {
            plan.add_sync_stage(
                "grouped outputs: single-sample slips when a member's clock drifts (≤ 0.1 %)",
            );
        }
        Ok(Box::new(plan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_bodies_and_mime_hints() {
        assert_eq!(
            first_stream_url("#EXTM3U\n#EXTINF:-1,Jazz\nhttps://radio.example/stream.mp3\n").as_deref(),
            Some("https://radio.example/stream.mp3")
        );
        assert_eq!(
            first_stream_url("[playlist]\nNumberOfEntries=1\nFile1=http://radio.example:8000/\nTitle1=X\n")
                .as_deref(),
            Some("http://radio.example:8000/")
        );
        assert_eq!(first_stream_url("nothing here"), None);
        assert!(is_playlist("http://x/y.pls", ""));
        assert!(is_playlist("http://x/y", "audio/x-mpegurl; charset=utf-8"));
        assert!(!is_playlist("http://x/stream", "audio/mpeg"));
        assert_eq!(ext_for_mime("audio/mpeg"), Some("mp3"));
        assert_eq!(ext_for_mime("audio/aacp"), Some("aac"));
        assert_eq!(ext_for_mime("text/html"), None);
        assert_eq!(ext_for_url("http://x/stream.AAC?x=1"), Some("aac"));
        assert_eq!(ext_for_url("http://x/live"), None);
    }

    #[test]
    fn stalled_streams_time_out_instead_of_hanging() {
        use std::io::Read;
        /// Two chunks, then silence forever.
        struct Stalling(u8);
        impl Read for Stalling {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 < 2 {
                    self.0 += 1;
                    buf[..3].copy_from_slice(b"abc");
                    return Ok(3);
                }
                std::thread::sleep(std::time::Duration::from_secs(60));
                Ok(0)
            }
        }
        let mut r = TimeoutReader::new(Box::new(Stalling(0)), std::time::Duration::from_millis(200));
        let mut all = Vec::new();
        let mut buf = [0u8; 2];
        let err = loop {
            match r.read(&mut buf) {
                Ok(0) => panic!("should not end cleanly"),
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(e) => break e,
            }
        };
        assert_eq!(all, b"abcabc");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        // A clean end is a clean end.
        let mut r = TimeoutReader::new(
            Box::new(std::io::Cursor::new(b"xyz".to_vec())),
            std::time::Duration::from_secs(5),
        );
        let mut out = String::new();
        r.read_to_string(&mut out).unwrap();
        assert_eq!(out, "xyz");
    }

    #[test]
    fn icy_reader_strips_metadata_and_keeps_titles() {
        use std::io::Read;
        // 10 audio bytes, a 32-byte metadata block, 10 audio, an empty block, 5 audio.
        let mut meta = b"StreamTitle='Nils Frahm - Says';".to_vec();
        meta.resize(32, 0);
        let mut raw = Vec::new();
        raw.extend_from_slice(&[1u8; 10]);
        raw.push(2);
        raw.extend_from_slice(&meta);
        raw.extend_from_slice(&[2u8; 10]);
        raw.push(0);
        raw.extend_from_slice(&[3u8; 5]);
        let title: SharedTitle = Arc::new(Mutex::new(None));
        let mut r = IcyReader::new(std::io::Cursor::new(raw), 10, title.clone());
        let mut out = Vec::new();
        let mut buf = [0u8; 7];
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        let mut expect = vec![1u8; 10];
        expect.extend_from_slice(&[2u8; 10]);
        expect.extend_from_slice(&[3u8; 5]);
        assert_eq!(out, expect);
        assert_eq!(title.lock().unwrap().as_deref(), Some("Nils Frahm - Says"));
        assert_eq!(parse_icy_title(b"StreamTitle='';StreamUrl='';\0\0"), None);
        assert_eq!(
            parse_icy_title(b"StreamTitle='It's Alright - X';StreamUrl='';").as_deref(),
            Some("It's Alright - X")
        );
    }
}
