use super::*;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A zone that records what it is asked and plays along.
#[derive(Default)]
struct FakeHost {
    view: Mutex<View>,
    acts: Mutex<Vec<Act>>,
}

#[async_trait::async_trait]
impl Host for FakeHost {
    fn zones(&self) -> Vec<(String, String)> {
        vec![("kitchen".into(), "Kitchen".into())]
    }
    fn view(&self, zone: &str) -> Option<View> {
        (zone == "kitchen").then(|| self.view.lock().unwrap().clone())
    }
    async fn act(&self, _zone: &str, act: Act) -> Result<(), String> {
        let mut v = self.view.lock().unwrap();
        match &act {
            Act::SetUri { uri, meta } => {
                v.uri = uri.clone();
                v.meta = meta.clone();
                v.state = Transport::Stopped;
            }
            Act::Play => v.state = Transport::Playing,
            Act::Pause => v.state = Transport::Paused,
            Act::Volume(x) => v.volume = *x,
            Act::Seek(ms) => v.position_ms = *ms,
            _ => {}
        }
        self.acts.lock().unwrap().push(act);
        Ok(())
    }
}

fn soap(service: &str, action: &str, args: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
         <u:{action} xmlns:u=\"{service}\"><InstanceID>0</InstanceID>{args}</u:{action}></s:Body></s:Envelope>"
    )
}

async fn post(app: &Router, path: &str, body: String) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "text/xml")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = resp.status();
    (
        st,
        String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap(),
    )
}

fn renderer() -> (Arc<Renderer>, Arc<FakeHost>) {
    let host = Arc::new(FakeHost::default());
    (Renderer::new(host.clone(), "node-1", "0.1.0"), host)
}

#[tokio::test]
async fn describes_and_plays_a_uri() {
    let (r, host) = renderer();
    let app = r.router();
    let resp = app
        .clone()
        .oneshot(
            Request::get("/upnp/zone/kitchen/desc.xml")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let desc = String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(desc.contains(&format!("<UDN>uuid:{}</UDN>", r.udn("kitchen"))));
    assert!(desc.contains("<friendlyName>Kitchen</friendlyName>"));
    assert_eq!(r.udn("kitchen"), r.udn("kitchen"), "stable");
    assert_ne!(r.udn("kitchen"), r.udn("study"));
    for svc in ["avt", "rc", "cm"] {
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/upnp/zone/kitchen/{svc}.xml"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{svc}");
    }
    let resp = app
        .clone()
        .oneshot(
            Request::get("/upnp/zone/nowhere/desc.xml")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let didl = xml::esc(
        "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><item><dc:title>Get Lucky</dc:title></item></DIDL-Lite>",
    );
    let (st, body) = post(
        &app,
        "/upnp/zone/kitchen/ctl/avt",
        soap(
            xml::AVT,
            "SetAVTransportURI",
            &format!("<CurrentURI>http://192.168.1.20:8097/flow/1.flac</CurrentURI><CurrentURIMetaData>{didl}</CurrentURIMetaData>"),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.contains("SetAVTransportURIResponse"));
    post(
        &app,
        "/upnp/zone/kitchen/ctl/avt",
        soap(xml::AVT, "Play", "<Speed>1</Speed>"),
    )
    .await;
    let (_, info) = post(
        &app,
        "/upnp/zone/kitchen/ctl/avt",
        soap(xml::AVT, "GetTransportInfo", ""),
    )
    .await;
    assert!(
        info.contains("<CurrentTransportState>PLAYING</CurrentTransportState>"),
        "{info}"
    );
    let (_, pos) = post(
        &app,
        "/upnp/zone/kitchen/ctl/avt",
        soap(xml::AVT, "GetPositionInfo", ""),
    )
    .await;
    assert!(
        pos.contains("<TrackURI>http://192.168.1.20:8097/flow/1.flac</TrackURI>"),
        "{pos}"
    );
    assert!(pos.contains("Get Lucky"), "{pos}");
    post(
        &app,
        "/upnp/zone/kitchen/ctl/avt",
        soap(xml::AVT, "Seek", "<Unit>REL_TIME</Unit><Target>0:01:05</Target>"),
    )
    .await;
    post(
        &app,
        "/upnp/zone/kitchen/ctl/rc",
        soap(
            xml::RC,
            "SetVolume",
            "<Channel>Master</Channel><DesiredVolume>35</DesiredVolume>",
        ),
    )
    .await;
    let (_, vol) = post(
        &app,
        "/upnp/zone/kitchen/ctl/rc",
        soap(xml::RC, "GetVolume", "<Channel>Master</Channel>"),
    )
    .await;
    assert!(vol.contains("<CurrentVolume>35</CurrentVolume>"), "{vol}");
    let (_, proto) = post(
        &app,
        "/upnp/zone/kitchen/ctl/cm",
        soap(xml::CM, "GetProtocolInfo", ""),
    )
    .await;
    assert!(proto.contains("http-get:*:audio/flac:*"), "{proto}");

    // Not an http URI, and an unknown action: UPnP errors, nothing done.
    let (st, f) = post(
        &app,
        "/upnp/zone/kitchen/ctl/avt",
        soap(
            xml::AVT,
            "SetAVTransportURI",
            "<CurrentURI>file:///etc/passwd</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
        ),
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(f.contains("<errorCode>714</errorCode>"), "{f}");
    let (_, f) = post(&app, "/upnp/zone/kitchen/ctl/avt", soap(xml::AVT, "Record", "")).await;
    assert!(f.contains("<errorCode>401</errorCode>"));

    let acts = host.acts.lock().unwrap().clone();
    assert_eq!(acts.len(), 4, "{acts:?}");
    assert!(
        matches!(&acts[0], Act::SetUri { uri, meta } if uri == "http://192.168.1.20:8097/flow/1.flac" && meta.title.as_deref() == Some("Get Lucky"))
    );
    assert_eq!(acts[1], Act::Play);
    assert_eq!(acts[2], Act::Seek(65_000));
    assert_eq!(acts[3], Act::Volume(35));
}

#[tokio::test]
async fn subscribers_get_the_state_and_its_changes() {
    let (r, host) = renderer();
    let app = r.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cb = format!("http://{}/cb", listener.local_addr().unwrap());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("SUBSCRIBE")
                .uri("/upnp/zone/kitchen/evt/avt")
                .header("CALLBACK", format!("<{cb}>"))
                // A controller on this machine may have its callback on loopback
                .extension(Peer("127.0.0.1".parse().unwrap()))
                .header("NT", "upnp:event")
                .header("TIMEOUT", "Second-300")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sid = resp.headers()["SID"].to_str().unwrap().to_string();
    assert!(sid.starts_with("uuid:"));
    assert_eq!(resp.headers()["TIMEOUT"], "Second-300");

    async fn next_notify(l: &tokio::net::TcpListener) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut s, _) = tokio::time::timeout(Duration::from_secs(5), l.accept())
            .await
            .unwrap()
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = s.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(i) = text.find("\r\n\r\n") {
                let len: usize = text
                    .lines()
                    .find_map(|l| l.strip_prefix("CONTENT-LENGTH: "))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= i + 4 + len {
                    s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .unwrap();
                    return text;
                }
            }
            if n == 0 {
                return text;
            }
        }
    }

    r.notify_changes().await;
    let first = next_notify(&listener).await;
    assert!(first.starts_with("NOTIFY /cb HTTP/1.1"), "{first}");
    assert!(
        first.contains(&format!("SID: {sid}")) && first.contains("SEQ: 0"),
        "{first}"
    );
    assert!(
        first.contains("TransportState val=&quot;STOPPED&quot;"),
        "{first}"
    );

    // No change: nothing sent. A change: SEQ 1 with the new state.
    r.notify_changes().await;
    host.view.lock().unwrap().state = Transport::Playing;
    r.notify_changes().await;
    let second = next_notify(&listener).await;
    assert!(
        second.contains("SEQ: 1") && second.contains("val=&quot;PLAYING&quot;"),
        "{second}"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("UNSUBSCRIBE")
                .uri("/upnp/zone/kitchen/evt/avt")
                .header("SID", &sid)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(r.subs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn uris_and_callbacks_only_reach_what_the_controller_could() {
    let lan: Option<IpAddr> = Some("192.168.1.30".parse().unwrap());
    let local: Option<IpAddr> = Some("127.0.0.1".parse().unwrap());
    for ok in [
        "http://192.168.1.20:8097/a.flac",
        "https://1.1.1.1/x",
        "http://[fd00::4]:80/",
        "HTTP://10.0.0.2",
    ] {
        assert!(reachable_for(ok, lan).await.is_ok(), "{ok}");
    }
    for bad in [
        "http://127.0.0.1:7788/v1/info",
        "http://localhost:7788/",
        "http://[::1]/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::ffff:127.0.0.1]/",
        "http://0.0.0.0:22/",
        "http://224.0.0.1/",
        "http://192.168.1.20@127.0.0.1:7788/",
        "file:///etc/passwd",
        "http://no-such-host.invalid/a.flac",
    ] {
        assert!(reachable_for(bad, lan).await.is_err(), "{bad}");
    }
    // Loopback only for a controller on this machine; with no peer known, never
    assert!(reachable_for("http://127.0.0.1:8097/a.flac", local).await.is_ok());
    assert!(reachable_for("http://127.0.0.1:8097/a.flac", None).await.is_err());
    assert!(reachable_for("http://169.254.169.254/", local).await.is_err());

    // Through SOAP: refused with a fault, the zone never asked
    let (r, host) = renderer();
    let app = r.router();
    let set = |uri: &str| {
        soap(
            xml::AVT,
            "SetAVTransportURI",
            &format!("<CurrentURI>{uri}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>"),
        )
    };
    let (_, f) = post(
        &app,
        "/upnp/zone/kitchen/ctl/avt",
        set("http://169.254.169.254/latest/meta-data/"),
    )
    .await;
    assert!(f.contains("<errorCode>716</errorCode>"), "{f}");
    let next = soap(
        xml::AVT,
        "SetNextAVTransportURI",
        "<NextURI>file:///etc/passwd</NextURI><NextURIMetaData></NextURIMetaData>",
    );
    let (_, f) = post(&app, "/upnp/zone/kitchen/ctl/avt", next).await;
    assert!(f.contains("<errorCode>714</errorCode>"), "{f}");
    assert!(host.acts.lock().unwrap().is_empty());

    // A subscriber whose only callback is the machine's own port, from the LAN: refused
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("SUBSCRIBE")
                .uri("/upnp/zone/kitchen/evt/avt")
                .header("CALLBACK", "<http://127.0.0.1:7788/v1/zones>")
                .header("NT", "upnp:event")
                .extension(Peer("192.168.1.30".parse().unwrap()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}
