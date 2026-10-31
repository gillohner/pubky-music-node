//! One Cast session: connect, launch the default media receiver, load the
//! stream, stop. Generic over the byte stream so tests run it over plain
//! TCP against a fake receiver; production wraps TLS around a TCP socket.

use crate::proto::{decode, frame, CastMessage};
use crate::{CastError, DEFAULT_MEDIA_RECEIVER};
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
const SENDER: &str = "sender-pubky-music";
const RECEIVER_0: &str = "receiver-0";
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// The crate's own runtime: Cast I/O is async, callers (the zone's audio
/// thread, API handlers) are not necessarily on a runtime.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("pm-cast")
            .build()
            .expect("cast runtime")
    })
}

/// Run `fut` to completion from any context.
pub fn block_on<F: std::future::Future + Send + 'static>(fut: F) -> F::Output
where
    F::Output: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        // Inside another runtime: hand it to ours on a plain thread.
        std::thread::scope(|s| s.spawn(|| runtime().block_on(fut)).join().expect("cast thread"))
    } else {
        runtime().block_on(fut)
    }
}

pub struct CastLink<S> {
    stream: S,
    request_id: u64,
    /// Transport id of the launched media receiver app.
    pub transport_id: Option<String>,
    pub session_id: Option<String>,
    pub media_session_id: Option<u64>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> CastLink<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            request_id: 1,
            transport_id: None,
            session_id: None,
            media_session_id: None,
        }
    }

    pub(crate) async fn send(
        &mut self,
        destination: &str,
        namespace: &str,
        mut payload: Value,
    ) -> Result<u64, CastError> {
        let id = self.request_id;
        self.request_id += 1;
        if let Value::Object(m) = &mut payload {
            m.insert("requestId".into(), json!(id));
        }
        let msg = CastMessage::text(SENDER, destination, namespace, &payload.to_string());
        self.stream.write_all(&frame(&msg)).await?;
        self.stream.flush().await?;
        Ok(id)
    }

    pub(crate) async fn recv(&mut self) -> Result<CastMessage, CastError> {
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).await?;
        let len = u32::from_be_bytes(len) as usize;
        if len > 1 << 20 {
            return Err(CastError::Protocol(format!("frame of {len} bytes")));
        }
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body).await?;
        decode(&body)
    }

    /// Read until a message with `type` arrives (answering heartbeats),
    /// returning its JSON.
    async fn wait_for(&mut self, type_: &str, what: &'static str) -> Result<Value, CastError> {
        let deadline = tokio::time::Instant::now() + REPLY_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Err(CastError::Timeout(what));
            }
            let msg = tokio::time::timeout(left, self.recv())
                .await
                .map_err(|_| CastError::Timeout(what))??;
            let Some(text) = msg.payload_utf8.as_deref() else {
                continue;
            };
            let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
            let t = v.get("type").and_then(Value::as_str).unwrap_or("");
            if msg.namespace == NS_HEARTBEAT && t == "PING" {
                let src = msg.source_id.clone();
                self.send(&src, NS_HEARTBEAT, json!({"type": "PONG"})).await?;
                continue;
            }
            if t == "LAUNCH_ERROR" || t == "LOAD_FAILED" || t == "INVALID_REQUEST" {
                return Err(CastError::Receiver(format!(
                    "{t}: {}",
                    v.get("reason").and_then(Value::as_str).unwrap_or("")
                )));
            }
            if t == type_ {
                return Ok(v);
            }
        }
    }

    /// CONNECT to the device, launch the default media receiver, CONNECT to it.
    pub async fn launch(&mut self) -> Result<(), CastError> {
        self.send(RECEIVER_0, NS_CONNECTION, json!({"type": "CONNECT"}))
            .await?;
        self.send(
            RECEIVER_0,
            NS_RECEIVER,
            json!({"type": "LAUNCH", "appId": DEFAULT_MEDIA_RECEIVER}),
        )
        .await?;
        let status = loop {
            let v = self
                .wait_for("RECEIVER_STATUS", "RECEIVER_STATUS after LAUNCH")
                .await?;
            let apps = v
                .pointer("/status/applications")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(app) = apps
                .iter()
                .find(|a| a.get("appId").and_then(Value::as_str) == Some(DEFAULT_MEDIA_RECEIVER))
            {
                break app.clone();
            }
        };
        let transport = status
            .get("transportId")
            .and_then(Value::as_str)
            .ok_or_else(|| CastError::Protocol("no transportId".into()))?
            .to_string();
        self.session_id = status
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.send(&transport, NS_CONNECTION, json!({"type": "CONNECT"}))
            .await?;
        self.transport_id = Some(transport);
        Ok(())
    }

    /// On a fresh link: CONNECT to the device and, if it still runs `session_id`, join it (CONNECT
    /// to its transport). `Ok(false)` when the session is gone.
    pub async fn rejoin(&mut self, session_id: Option<&str>) -> Result<bool, CastError> {
        self.send(RECEIVER_0, NS_CONNECTION, json!({"type": "CONNECT"}))
            .await?;
        self.send(RECEIVER_0, NS_RECEIVER, json!({"type": "GET_STATUS"}))
            .await?;
        let v = self
            .wait_for("RECEIVER_STATUS", "RECEIVER_STATUS after GET_STATUS")
            .await?;
        let Some(sid) = session_id else {
            return Ok(false);
        };
        let apps = v
            .pointer("/status/applications")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(app) = apps
            .iter()
            .find(|a| a.get("sessionId").and_then(Value::as_str) == Some(sid))
        else {
            return Ok(false);
        };
        let transport = app
            .get("transportId")
            .and_then(Value::as_str)
            .ok_or_else(|| CastError::Protocol("no transportId".into()))?
            .to_string();
        self.send(&transport, NS_CONNECTION, json!({"type": "CONNECT"}))
            .await?;
        self.session_id = Some(sid.to_string());
        self.transport_id = Some(transport);
        Ok(true)
    }

    /// LOAD a live stream URL with autoplay, showing `meta` on the device.
    pub async fn load(&mut self, url: &str, content_type: &str, meta: &crate::Meta) -> Result<(), CastError> {
        self.load_with(url, content_type, meta, true).await
    }

    /// [`Self::load`], playing at once only with `autoplay` (else [`Self::play`] starts it).
    pub async fn load_with(
        &mut self,
        url: &str,
        content_type: &str,
        meta: &crate::Meta,
        autoplay: bool,
    ) -> Result<(), CastError> {
        let transport = self
            .transport_id
            .clone()
            .ok_or_else(|| CastError::Protocol("not launched".into()))?;
        self.send(
            &transport,
            NS_MEDIA,
            json!({
                "type": "LOAD",
                "autoplay": autoplay,
                "currentTime": 0,
                "media": {
                    "contentId": url,
                    "contentType": content_type,
                    "streamType": "LIVE",
                    "metadata": crate::meta_json(meta, "Pubky Music")
                }
            }),
        )
        .await?;
        let v = self.wait_for("MEDIA_STATUS", "MEDIA_STATUS after LOAD").await?;
        self.media_session_id = v.pointer("/status/0/mediaSessionId").and_then(Value::as_u64);
        Ok(())
    }

    /// PLAY the loaded media.
    pub async fn play(&mut self) -> Result<(), CastError> {
        let (Some(transport), Some(m)) = (self.transport_id.clone(), self.media_session_id) else {
            return Err(CastError::Protocol("nothing loaded".into()));
        };
        self.send(&transport, NS_MEDIA, json!({"type": "PLAY", "mediaSessionId": m}))
            .await
            .map(drop)
    }

    /// Stop the app (receiver STOP), which also ends the stream fetch.
    pub async fn stop(&mut self) -> Result<(), CastError> {
        if let Some(session) = self.session_id.clone() {
            self.send(
                RECEIVER_0,
                NS_RECEIVER,
                json!({"type": "STOP", "sessionId": session}),
            )
            .await?;
        }
        Ok(())
    }

    /// Device volume level 0.0..=1.0, or mute.
    pub async fn set_volume(&mut self, level: Option<f64>, muted: Option<bool>) -> Result<(), CastError> {
        let mut vol = serde_json::Map::new();
        if let Some(l) = level {
            vol.insert("level".into(), json!(l.clamp(0.0, 1.0)));
        }
        if let Some(m) = muted {
            vol.insert("muted".into(), json!(m));
        }
        self.send(
            RECEIVER_0,
            NS_RECEIVER,
            json!({"type": "SET_VOLUME", "volume": Value::Object(vol)}),
        )
        .await?;
        Ok(())
    }

    /// Keep answering heartbeats; returns when the peer closes.
    pub async fn serve_heartbeat(&mut self) -> Result<(), CastError> {
        loop {
            let msg = self.recv().await?;
            if msg.namespace == NS_HEARTBEAT {
                let src = msg.source_id.clone();
                self.send(&src, NS_HEARTBEAT, json!({"type": "PONG"})).await?;
            }
        }
    }
}

/// TLS client config that accepts the device's self-signed certificate.
/// Cast devices do not present chains a public CA signed; the trust model
/// is the LAN, as for every other Cast sender.
pub fn permissive_tls() -> std::sync::Arc<rustls::ClientConfig> {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    #[derive(Debug)]
    struct NoVerify;
    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ED25519,
            ]
        }
    }
    let mut cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(NoVerify))
        .with_no_client_auth();
    cfg.alpn_protocols = Vec::new();
    std::sync::Arc::new(cfg)
}

/// Open a TLS Cast link to `addr`.
pub async fn connect_tls(
    addr: std::net::SocketAddr,
) -> Result<CastLink<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>, CastError> {
    let tcp = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_| CastError::Timeout("TCP connect"))??;
    tcp.set_nodelay(true)?;
    let connector = tokio_rustls::TlsConnector::from(permissive_tls());
    let name = rustls::pki_types::ServerName::IpAddress(addr.ip().into());
    let tls = connector
        .connect(name, tcp)
        .await
        .map_err(|e| CastError::Tls(e.to_string()))?;
    Ok(CastLink::new(tls))
}

impl<S> CastLink<S> {
    /// The byte stream and the next request id, for a session that takes the link over.
    pub(crate) fn into_parts(self) -> (S, u64) {
        (self.stream, self.request_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    /// A fake default-media-receiver speaking plain TCP: answers CONNECT
    /// silently, LAUNCH with RECEIVER_STATUS, LOAD with MEDIA_STATUS, logs
    /// everything, sends a PING once.
    async fn fake_receiver(listener: TcpListener, log: Arc<Mutex<Vec<String>>>) {
        let (sock, _) = listener.accept().await.unwrap();
        let mut link = CastLink::new(sock);
        let mut pinged = false;
        loop {
            let msg = match link.recv().await {
                Ok(m) => m,
                Err(_) => return,
            };
            let v: Value = serde_json::from_str(msg.payload_utf8.as_deref().unwrap_or("{}")).unwrap();
            let t = v["type"].as_str().unwrap_or("").to_string();
            log.lock()
                .unwrap()
                .push(format!("{} {}", msg.namespace.rsplit('.').next().unwrap(), t));
            let rid = v.get("requestId").cloned().unwrap_or(json!(0));
            match t.as_str() {
                "LAUNCH" => {
                    if !pinged {
                        pinged = true;
                        link.send("receiver-0", NS_HEARTBEAT, json!({"type": "PING"}))
                            .await
                            .unwrap();
                    }
                    link.send(
                        &msg.source_id,
                        NS_RECEIVER,
                        json!({"type": "RECEIVER_STATUS", "requestId": rid, "status": {"applications": [{"appId": DEFAULT_MEDIA_RECEIVER, "transportId": "web-7", "sessionId": "sess-1"}]}}),
                    )
                    .await
                    .unwrap();
                }
                "LOAD" => {
                    log.lock()
                        .unwrap()
                        .push(format!("url {}", v["media"]["contentId"].as_str().unwrap()));
                    link.send(
                        &msg.source_id,
                        NS_MEDIA,
                        json!({"type": "MEDIA_STATUS", "requestId": rid, "status": [{"mediaSessionId": 9, "playerState": "BUFFERING"}]}),
                    )
                    .await
                    .unwrap();
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn launch_load_stop_against_a_fake_receiver() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(fake_receiver(listener, log.clone()));
        let sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut link = CastLink::new(sock);
        link.launch().await.unwrap();
        assert_eq!(link.transport_id.as_deref(), Some("web-7"));
        assert_eq!(link.session_id.as_deref(), Some("sess-1"));
        link.load(
            "http://node:7788/v1/outputs/pull/cast-x/stream.wav",
            "audio/wav",
            &crate::Meta::default(),
        )
        .await
        .unwrap();
        assert_eq!(link.media_session_id, Some(9));
        link.set_volume(Some(0.5), None).await.unwrap();
        link.stop().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let l = log.lock().unwrap().clone();
        assert_eq!(
            l,
            vec![
                "connection CONNECT",
                "receiver LAUNCH",
                "heartbeat PONG",
                "connection CONNECT",
                "media LOAD",
                "url http://node:7788/v1/outputs/pull/cast-x/stream.wav",
                "receiver SET_VOLUME",
                "receiver STOP"
            ]
        );
    }
}
