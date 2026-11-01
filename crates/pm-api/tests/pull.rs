//! A zone playing into a pull output: the HTTP WAV stream is the exact PCM
//! of the source, and a listener is required. A fake Linn DS (OpenHome) fetches it the way
//! Linn's pipeline does.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::{OutputRegistry, ZoneId};
use tower::ServiceExt;

fn write_wav(path: &std::path::Path, frames: usize) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    let mut pcm = Vec::new();
    for i in 0..frames * 2 {
        let s = ((i as i64 * 7919) % (1 << 24)) as i32 - (1 << 23);
        w.write_sample(s).unwrap();
        pcm.extend_from_slice(&s.to_le_bytes()[..3]);
    }
    w.finalize().unwrap();
    pcm
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_stream_is_the_exact_pcm() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let pcm = write_wav(&wav, 20_000);
    let db = dir.path().join("lib.sqlite");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: None,
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://127.0.0.1:7788".into(),
            upnp: false,
            cast: false,
            slimproto_port: 0,
            session_secret_path: None,
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    );
    let app = router(s.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/outputs/pull")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"living"}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let zone = s.zones.get(&ZoneId::new("stream-living")).unwrap();
    assert_eq!(zone.output().id().0, "pull:living");

    // Enqueue the file and play; the sink waits for a listener.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/zones/stream-living/queue")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"items":[{{"uri":"{}"}}],"replace":true,"play":true}}"#,
            wav.display()
        )))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/outputs/pull/living/stream.wav")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "audio/wav");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[0..4], b"RIFF");
    assert_eq!(&body[36..40], b"data");
    assert_eq!(&body[44..], &pcm[..], "stream PCM differs from the source");
    let st = zone.wait_stopped().await;
    assert_eq!(st.error, None);

    // Unknown stream and duplicate names.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/outputs/pull/nope/stream.wav")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/outputs/pull")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"living"}"#))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::CONFLICT
    );
}

/// With an API token set, renderers open a pull stream with that stream's key (`?k=`), which
/// opens nothing else; the token still works too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renderers_open_pull_streams_with_the_stream_key_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lib.sqlite");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: Some("secret".into()),
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://192.168.1.2:7788".into(),
            upnp: false,
            cast: false,
            slimproto_port: 0,
            session_secret_path: None,
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    );
    let app = router(s.clone());
    let get = |uri: String| {
        let app = app.clone();
        async move {
            app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
    };
    let req = Request::builder()
        .method("POST")
        .uri("/v1/outputs/pull")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(Body::from(r#"{"name":"tv"}"#))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);

    // The URL handed out carries the key, never the token.
    let url = s.pull_url("tv");
    assert!(
        url.starts_with("http://192.168.1.2:7788/v1/outputs/pull/tv/stream.wav?k="),
        "{url}"
    );
    assert!(!url.contains("secret"));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/outputs/pull/tv/url")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["url"],
        url
    );

    // No key, a wrong key, or another stream's key: refused.
    assert_eq!(
        get("/v1/outputs/pull/tv/stream.wav".into()).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get("/v1/outputs/pull/tv/stream.wav?k=00".into()).await,
        StatusCode::UNAUTHORIZED
    );
    let other = s.stream_key("kitchen");
    assert_eq!(
        get(format!("/v1/outputs/pull/tv/stream.wav?k={other}")).await,
        StatusCode::UNAUTHORIZED
    );
    // The key opens nothing but its stream.
    let key = s.stream_key("tv");
    assert_eq!(get(format!("/v1/info?k={key}")).await, StatusCode::UNAUTHORIZED);
    // The right key passes (nothing plays yet, so the stream waits for a zone and gives up).
    let nope = s.stream_key("nope");
    assert_eq!(
        get(format!("/v1/outputs/pull/nope/stream.wav?k={nope}")).await,
        StatusCode::NOT_FOUND
    );
    // Covers for TV screens: their own key per album.
    let art = s.art_url(7);
    assert!(art.starts_with("http://192.168.1.2:7788/v1/art/7?k="), "{art}");
    let path = art.trim_start_matches("http://192.168.1.2:7788").to_string();
    assert_eq!(get(path).await, StatusCode::NOT_FOUND); // right key, no such album
    assert_eq!(get(format!("/v1/art/7?k={key}")).await, StatusCode::UNAUTHORIZED);
    // Keys survive a restart: they come from a secret kept in the library.
    drop(app);
    let again = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: Some("secret".into()),
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://192.168.1.2:7788".into(),
            upnp: false,
            cast: false,
            slimproto_port: 0,
            session_secret_path: None,
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    );
    assert_eq!(again.stream_key("tv"), key);
}

/// What Linn's WAV codec (ohPipeline `CodecWav`) makes of a stream's start, ported line by line:
/// where the audio starts, and how many bytes it plays (`None`: a continuous stream). Chunk sizes
/// are 32-bit and an odd one is rounded up, so `0xFFFFFFFF` wraps to 0.
fn openhome_wav(b: &[u8]) -> Result<(usize, Option<u32>), String> {
    let u32_at = |i: usize| -> Result<u32, String> {
        b.get(i..i + 4)
            .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
            .ok_or_else(|| "stream ended in the header".to_string())
    };
    if b.get(0..4) != Some(b"RIFF") || b.get(8..12) != Some(b"WAVE") {
        return Err("not recognised as WAV".into());
    }
    let file_size = u32_at(4)?; // 0: continuous stream
    let mut i = 12;
    let mut find = |id: &[u8]| -> Result<(usize, u32), String> {
        loop {
            let bytes = u32_at(i + 4)?;
            let bytes = bytes.wrapping_add(bytes % 2);
            let at = i + 8;
            if b.get(i..i + 4) == Some(id) {
                i = at + bytes as usize;
                return Ok((at, bytes));
            }
            i = at + bytes as usize;
        }
    };
    let (fmt_at, fmt_bytes) = find(b"fmt ")?;
    if ![16, 18, 40].contains(&fmt_bytes) {
        return Err("CodecStreamCorrupt: fmt size".into());
    }
    let audio_format = u16::from_le_bytes([b[fmt_at], b[fmt_at + 1]]);
    if audio_format != 1 && audio_format != 0xfffe {
        return Err("CodecStreamFeatureUnsupported".into());
    }
    let channels = u16::from_le_bytes([b[fmt_at + 2], b[fmt_at + 3]]) as u32;
    let bits = u16::from_le_bytes([b[fmt_at + 14], b[fmt_at + 15]]) as u32;
    let sample_bytes = channels * (bits / 8);
    let (data_at, data_bytes) = find(b"data")?;
    let total = (file_size != 0).then(|| data_bytes - data_bytes % sample_bytes);
    Ok((data_at, total))
}

#[test]
fn the_usual_live_wav_header_ends_at_once_on_a_linn_and_the_continuous_one_plays() {
    let fmt = pm_model::AudioFormat::pcm(44_100, 16, 2);
    let (_, plays) = openhome_wav(&pm_zones::outputs::wav_header(fmt)).unwrap();
    assert_eq!(
        plays,
        Some(0),
        "0xFFFFFFFF wraps: the Linn ends the stream before its first sample"
    );
    let (at, plays) = openhome_wav(&pm_zones::outputs::wav_header_continuous(fmt)).unwrap();
    assert_eq!((at, plays), (44, None), "RIFF size 0: continuous");
}

/// One HTTP/1.1 GET as Linn's pipeline sends it (or, with `keep_alive`, as most other clients
/// do: no `Connection: close`); returns the status line, the headers and a reader positioned at
/// the body (de-chunked when the answer is chunked).
fn linn_get(
    addr: std::net::SocketAddr,
    path: &str,
    keep_alive: bool,
) -> (String, Vec<String>, Box<dyn std::io::Read>) {
    use std::io::{BufRead, BufReader, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: LWIP/1.4.2-linn\r\n{}Icy-MetaData: 1\r\nRange: bytes=0-\r\n\r\n",
        if keep_alive { "" } else { "Connection: close\r\n" }
    )
    .unwrap();
    let mut r = BufReader::new(s);
    let mut status = String::new();
    r.read_line(&mut status).unwrap();
    let mut headers = Vec::new();
    loop {
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        if l == "\r\n" || l.is_empty() {
            break;
        }
        headers.push(l.trim().to_ascii_lowercase());
    }
    let chunked = headers.iter().any(|h| h == "transfer-encoding: chunked");
    // A Linn DS stops on a chunked live stream (the owner's Klimax DS, 0.2.0)
    assert!(
        !(chunked && path.ends_with("live.wav")),
        "a Linn does not play a chunked stream: {status} {headers:?}"
    );
    let body: Box<dyn std::io::Read> = if chunked {
        Box::new(Dechunk {
            r,
            left: 0,
            done: false,
        })
    } else {
        Box::new(r)
    };
    (status.trim().to_string(), headers, body)
}

/// HTTP/1.1 chunked transfer decoding (Linn's `ReaderHttpChunked`).
struct Dechunk<R: std::io::BufRead> {
    r: R,
    left: usize,
    done: bool,
}

impl<R: std::io::BufRead> std::io::Read for Dechunk<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            self.r.read_line(&mut line)?;
            if line == "\r\n" {
                line.clear();
                self.r.read_line(&mut line)?;
            }
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|e| std::io::Error::other(format!("chunk size {line:?}: {e}")))?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.left = size;
        }
        let n = buf.len().min(self.left);
        let n = self.r.read(&mut buf[..n])?;
        self.left -= n;
        Ok(n)
    }
}

/// A fake Linn DS: it fetches a live stream as Linn's pipeline does (ohPipeline `ProtocolHttp`).
/// No Content-Length means live: it reads the start to recognise the codec, closes the
/// connection, and opens it again once its pipeline is ready. The second connection must get the
/// stream from the top, with a header its WAV codec reads as continuous, and every sample.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linn_that_reconnects_after_reading_the_start_plays_every_sample() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let pcm = write_wav(&wav, 40_000);
    let db = dir.path().join("lib.sqlite");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: None,
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://127.0.0.1:7788".into(),
            upnp: false,
            cast: false,
            slimproto_port: 0,
            session_secret_path: None,
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    );
    let app = router(s.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/outputs/pull")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"linn"}"#))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(pm_api::serve_listener(s.clone(), listener));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/zones/stream-linn/queue")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"items":[{{"uri":"{}"}}],"replace":true,"play":true}}"#,
            wav.display()
        )))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);

    let heard = tokio::task::spawn_blocking(move || {
        let path = "/v1/outputs/pull/linn/live.wav";
        // First connection: recognise the stream from its first bytes, then hang up
        let (status, headers, mut body) = linn_get(addr, path, false);
        assert!(status.starts_with("HTTP/1.0 200"), "{status}");
        assert!(
            !headers.iter().any(|h| h.starts_with("content-length")),
            "live: no length {headers:?}"
        );
        assert!(
            headers.iter().any(|h| h.starts_with("content-type: audio/wav")),
            "{headers:?}"
        );
        let mut start = vec![0u8; 8 * 1024];
        body.read_exact(&mut start).unwrap();
        let (at, plays) = openhome_wav(&start).unwrap();
        assert_eq!(plays, None, "the Linn must see a continuous stream");
        assert_eq!(&start[at..at + 64], &pcm[..64]);
        drop(body);
        // The pipeline is ready: open it again and play. This time as a keep-alive client: the
        // answer is still HTTP/1.0, unchunked, says it closes, and ends when the zone stops.
        let (status, headers, mut body) = linn_get(addr, path, true);
        assert!(
            status.starts_with("HTTP/1.0 200"),
            "the stream waited for the Linn: {status}"
        );
        assert!(
            headers.iter().any(|h| h == "connection: close")
                && !headers.iter().any(|h| h.starts_with("transfer-encoding")),
            "{headers:?}"
        );
        let mut all = Vec::new();
        body.read_to_end(&mut all).unwrap();
        let (at, plays) = openhome_wav(&all).unwrap();
        assert_eq!(plays, None);
        (all.split_off(at), pcm)
    })
    .await
    .unwrap();
    let (got, pcm) = heard;
    assert_eq!(got.len(), pcm.len(), "every byte, from the first");
    assert!(got == pcm, "the PCM differs from the source");
    let zone = s.zones.get(&ZoneId::new("stream-linn")).unwrap();
    let st = zone.wait_stopped().await;
    assert_eq!(st.error, None, "{st:?}");
}

/// The stream an OpenHome renderer gets when the track fits (`sized.wav`): a WAV file of fixed
/// size, so a Linn's own Pause holds it (on a live stream it plays on). The answer is HTTP/1.0,
/// unchunked, with a Content-Length equal to the RIFF header's sizes plus the header; it ends
/// early when the zone stops, with every sample from the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sized_stream_says_its_length_and_carries_every_sample() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let pcm = write_wav(&wav, 40_000);
    let db = dir.path().join("lib.sqlite");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: None,
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://127.0.0.1:7788".into(),
            upnp: false,
            cast: false,
            slimproto_port: 0,
            session_secret_path: None,
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    );
    let app = router(s.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/outputs/pull")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"linn"}"#))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(pm_api::serve_listener(s.clone(), listener));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/zones/stream-linn/queue")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"items":[{{"uri":"{}"}}],"replace":true,"play":true}}"#,
            wav.display()
        )))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);

    let heard = tokio::task::spawn_blocking(move || {
        let (status, headers, mut body) = linn_get(addr, "/v1/outputs/pull/linn/sized.wav", true);
        assert!(status.starts_with("HTTP/1.0 200"), "{status}");
        assert!(
            !headers.iter().any(|h| h.starts_with("transfer-encoding")),
            "{headers:?}"
        );
        let fmt = pm_model::AudioFormat::pcm(48_000, 24, 2);
        let (_, len) = pm_zones::outputs::wav_header_sized_stream(fmt);
        assert!(
            headers.iter().any(|h| *h == format!("content-length: {len}")),
            "the length of the file: {headers:?}"
        );
        let mut all = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            match body.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
            }
        }
        let (at, plays) = openhome_wav(&all).unwrap();
        assert_eq!(at, 44);
        assert_eq!(plays.map(u64::from), Some(len - 44), "a file of that size");
        (all.split_off(at), pcm)
    })
    .await
    .unwrap();
    let (got, pcm) = heard;
    assert_eq!(got.len(), pcm.len(), "every byte, from the first");
    assert!(got == pcm, "the PCM differs from the source");
    let zone = s.zones.get(&ZoneId::new("stream-linn")).unwrap();
    let st = zone.wait_stopped().await;
    assert_eq!(st.error, None, "{st:?}");
}

/// A stopped Linn polls its old URI every 15 s, and the node held each request for a stream: one
/// that was waiting when the next stream opened took it, and the Linn dropped it ("listener went
/// away", NUC 2026-10-30). Now that request is answered 410 at once, and the fetch the Linn makes
/// after SetAVTransportURI gets the new stream from its first byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_linn_request_does_not_take_the_new_stream() {
    use pm_model::{AudioBlock, AudioFormat};
    use pm_zones::Output;
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lib.sqlite");
    let s = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: None,
            subsonic_user: "pubky".into(),
            pubky: Default::default(),
            public_url: "http://127.0.0.1:7788".into(),
            upnp: false,
            cast: false,
            slimproto_port: 0,
            session_secret_path: None,
            tidal: Default::default(),
            spotify: Default::default(),
            acoustid_key: String::new(),
            jamendo: Default::default(),
        },
        Library::open(&db).unwrap(),
        OutputRegistry::new(),
    );
    // The renderer's stream, as the node makes it for a UPnP renderer
    let pull = s.pull_output("klimax");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(pm_api::serve_listener(s.clone(), listener));
    let fmt = AudioFormat::pcm(44_100, 16, 2);
    let path = "/v1/outputs/pull/klimax/live.wav";
    // The last session ended; the stopped Linn asks again for its old URI
    drop(pm_stream::block_on(pull.open(fmt)).unwrap());
    let old = pull.generation();
    let stale = tokio::task::spawn_blocking(move || {
        let t = std::time::Instant::now();
        let (status, _, _) = linn_get(addr, path, false);
        (status, t.elapsed())
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // Play: the node opens the next stream, then the Linn fetches it
    let mut sink = pm_stream::block_on(pull.open(fmt)).unwrap();
    let samples: Vec<i32> = (0..4410 * 2).map(|i| (i % 1000) - 500).collect();
    let writer = std::thread::spawn(move || {
        sink.write(&AudioBlock::pcm_i32(fmt, samples)).unwrap();
        sink
    });
    let (status, took) = stale.await.unwrap();
    assert!(
        status.starts_with("HTTP/1.0 410") || status.starts_with("HTTP/1.1 410"),
        "{status}"
    );
    assert!(
        took < std::time::Duration::from_secs(2),
        "answered at once: {took:?}"
    );
    // It asks again at once, now after the new stream opened: its URL names the old stream, so
    // it is answered again (review of #181)
    let again = tokio::task::spawn_blocking(move || {
        let t = std::time::Instant::now();
        let (status, _, _) = linn_get(addr, &format!("{path}?g={old}"), false);
        (status, t.elapsed())
    })
    .await
    .unwrap();
    assert!(again.0.contains(" 410"), "{again:?}");
    assert!(again.1 < std::time::Duration::from_secs(2), "{again:?}");
    let new = pull.generation();
    let first = tokio::task::spawn_blocking(move || {
        let (status, _, mut body) = linn_get(addr, &format!("{path}?g={new}"), false);
        assert!(
            status.starts_with("HTTP/1.0 200"),
            "the fresh fetch plays: {status}"
        );
        let mut start = vec![0u8; 44 + 4];
        body.read_exact(&mut start).unwrap();
        start[44..].to_vec()
    })
    .await
    .unwrap();
    // The first sample, as the zone wrote it (16-bit LE)
    assert_eq!(first, [0x0c, 0xfe, 0x0d, 0xfe], "from the first byte");
    drop(writer.join().unwrap());
}
