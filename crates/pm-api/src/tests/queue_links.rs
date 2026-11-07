//! Links on the node in a zone's queue never carry a sign-in: a browser's cover link
//! (`/v1/library/albums/{id}/cover?token=…`) loses the token on the way into the queue, in every
//! answer to listeners (zones, queue, presence) and on a renderer's screen; a saved queue that
//! held one is cleaned when the node starts, and that sign-in ends.

use super::*;

const LISTENER: &str = "tbepfp8fisyiqpskbmxsmpub47gxb7xdsh5x9ut9qrb97gcq9phy";

async fn as_token(
    app: &Router,
    token: &str,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(
            body.map(|b| Body::from(b.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    let st = r.status();
    let b = r.into_body().collect().await.unwrap().to_bytes();
    (st, String::from_utf8_lossy(&b).into_owned())
}

fn let_in(s: &NodeState, key: &str) -> String {
    s.library
        .lock()
        .unwrap()
        .set_setting(
            "people",
            &serde_json::json!({"people": [{"key": key, "access": "music", "music_enabled": true, "added_at": 1}]})
                .to_string(),
        )
        .unwrap();
    accounts::mint(s, key).unwrap()
}

/// A queued cover link with the owner's sign-in: the queue keeps the link without it, so a
/// listener's reads of the zone, its queue, who is listening and the event stream, and a
/// renderer's screen, never see the sign-in. The cover itself still loads with `?token=`.
#[tokio::test]
async fn queue_links_never_carry_a_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    write_wav(&wav, 44100);
    let s = state(dir.path(), Some("s3cret")).await;
    let app = router(s.clone());
    let listener = let_in(&s, LISTENER);
    let (_, zones) = call_as(&app, "s3cret", "/v1/zones").await;
    let zid = zones[0]["id"].as_str().unwrap().to_string();
    s.library
        .lock()
        .unwrap()
        .set_setting("presence.visible.owner", "everyone")
        .unwrap();
    // The owner's browser queues a mixed list: a library row's cover link carries its sign-in
    let (st, _) = as_token(
        &app,
        "s3cret",
        "POST",
        &format!("/v1/zones/{zid}/queue"),
        Some(serde_json::json!({"items": [
            {"uri": wav.to_string_lossy(), "title": "A", "image": "/v1/library/albums/1/cover?token=s3cret"},
            {"uri": "https://radio.example/live?token=theirs", "title": "B", "image": "https://cdn.example/b.jpg"}
        ], "play": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    // The queue kept the link, not the sign-in; a link that isn't the node's is as it was
    let (_, q) = call_as(&app, "s3cret", &format!("/v1/zones/{zid}/queue")).await;
    assert_eq!(q[0]["image"], "/v1/library/albums/1/cover", "{q}");
    assert_eq!(q[1]["uri"], "https://radio.example/live?token=theirs", "{q}");
    // Playing: who is listening carries the item
    let playing = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if s.zones
                .get(&pm_zones::ZoneId(zid.clone()))
                .unwrap()
                .state()
                .now_playing
                .is_some()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(playing.is_ok(), "the zone plays");
    for path in [
        "/v1/zones".to_string(),
        format!("/v1/zones/{zid}"),
        format!("/v1/zones/{zid}/queue"),
        "/v1/presence".to_string(),
    ] {
        let (st, body) = as_token(&app, &listener, "GET", &path, None).await;
        assert_eq!(st, StatusCode::OK, "{path}: {body}");
        assert!(body.contains("/v1/library/albums/1/cover"), "{path}: {body}");
        assert!(!body.contains("s3cret"), "{path}: {body}");
    }
    // A renderer's screen gets the keyed art link, which it can fetch
    let describe = s.describe_for_renderers();
    let mut item = pm_zones::QueueItem::new("https://radio.example/live");
    item.image = Some("/v1/library/albums/1/cover".into());
    assert_eq!(describe(&item).image, Some(s.art_url(1)));
    item.image = Some("http://127.0.0.1:7788/v1/library/albums/1/cover".into());
    assert_eq!(
        describe(&item).image,
        Some(s.art_url(1)),
        "an absolute node cover link too"
    );
    item.image = Some("https://cdn.example/v1/library/albums/1/cover".into());
    assert_eq!(
        describe(&item).image.as_deref(),
        Some("https://cdn.example/v1/library/albums/1/cover"),
        "another host's picture is as it was"
    );
    item.image = Some("/v1/library/tracks/1/file".into());
    assert_eq!(
        describe(&item).image,
        None,
        "no other node link reaches a renderer"
    );
    // The cover still loads by address, for the owner and for a listener
    for t in ["s3cret", listener.as_str()] {
        let (st, _) = call(
            &app,
            "GET",
            &format!("/v1/library/albums/1/cover?token={t}"),
            None,
        )
        .await;
        assert_eq!(
            st,
            StatusCode::NOT_FOUND,
            "no art kept, but past the sign-in check"
        );
    }
}

async fn call_as(app: &Router, token: &str, path: &str) -> (StatusCode, serde_json::Value) {
    let (st, body) = as_token(app, token, "GET", path, None).await;
    (st, serde_json::from_str(&body).unwrap_or_default())
}

/// A queue saved by an older browser holds sign-ins in its links: at start they are removed from
/// the saved queue, the owner's sessions end once, a listener's token in it ends, and the API
/// token in it is reported (it can only be replaced by hand).
#[tokio::test]
async fn a_saved_queue_with_a_sign_in_is_cleaned_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let s = state(dir.path(), Some("s3cret")).await;
    let app = router(s.clone());
    let listener = let_in(&s, LISTENER);
    let (_, zones) = call_as(&app, "s3cret", "/v1/zones").await;
    let zid = zones[0]["id"].as_str().unwrap().to_string();
    let key = format!("queue.{zid}");
    // As saved by this node, then with the links an older browser wrote
    let saved = serde_json::json!({
        "items": [
            {"id": 1, "uri": "track:1", "image": format!("/v1/library/albums/1/cover?token={listener}")},
            {"id": 2, "uri": "/v1/library/tracks/2/file?token=s3cret", "image": "/v1/library/albums/2/cover?token=pmo_browser"},
            {"id": 3, "uri": "https://radio.example/live?token=theirs"},
            {"id": 4, "uri": "https://other-node.example/v1/library/tracks/9/file?token=pmo_theirs"},
            {"id": 5, "uri": "track:5", "image": "http://localhost:7788/v1/library/albums/5/cover?token=pmo_phone"}
        ],
        "repeat": "off", "radio": false, "shuffle": false, "order": []
    });
    let sessions = |name: &str| {
        serde_json::json!([{"hash": blake3::hash(name.as_bytes()).to_hex().to_string(), "key": "owner", "created_at": 1}])
            .to_string()
    };
    {
        let lib = s.library.lock().unwrap();
        lib.set_setting(&key, &saved.to_string()).unwrap();
        lib.set_setting("accounts.owner_sessions", &sessions("pmo_browser"))
            .unwrap();
        lib.set_setting("accounts.device_sessions", &sessions("pmo_phone"))
            .unwrap();
    }
    let cleaned = accounts::clean_saved_queues(&s).unwrap();
    assert_eq!(
        cleaned,
        accounts::CleanedQueues {
            items: 3,
            owner_sessions_ended: true,
            api_token_found: true,
            listener_tokens_ended: 1,
        }
    );
    let setting = |k: &str| {
        s.library
            .lock()
            .unwrap()
            .get_setting(k)
            .unwrap()
            .unwrap_or_default()
    };
    let json = setting(&key);
    for t in ["s3cret", listener.as_str()] {
        assert!(!json.contains(t), "{t}: {json}");
    }
    assert!(
        json.contains("\"/v1/library/albums/2/cover\""),
        "the node's link is cleaned: {json}"
    );
    assert!(json.contains("/v1/library/albums/1/cover\""), "{json}");
    assert!(
        json.contains("https://radio.example/live?token=theirs"),
        "not the node's link: {json}"
    );
    assert!(
        json.contains("https://other-node.example/v1/library/tracks/9/file?token=pmo_theirs"),
        "another node's link keeps its own token: {json}"
    );
    assert!(
        json.contains("\"http://localhost:7788/v1/library/albums/5/cover\""),
        "the node under another name loses its own sign-in: {json}"
    );
    assert_eq!(setting("accounts.owner_sessions"), "[]");
    assert_eq!(setting("accounts.device_sessions"), "[]");
    let (st, _) = as_token(&app, &listener, "GET", "/v1/info", None).await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "the listener's token in the queue ended"
    );
    // Nothing left to clean: a second start ends nothing
    assert_eq!(
        accounts::clean_saved_queues(&s).unwrap(),
        accounts::CleanedQueues::default()
    );
    // The node starts again: the restored queue is the cleaned one
    drop(app);
    drop(s);
    let s2 = state(dir.path(), Some("s3cret")).await;
    let app2 = router(s2.clone());
    let (_, q) = call_as(&app2, "s3cret", &format!("/v1/zones/{zid}/queue")).await;
    assert_eq!(q[0]["image"], "/v1/library/albums/1/cover", "{q}");
    assert_eq!(q[1]["uri"], "/v1/library/tracks/2/file", "{q}");
    assert_eq!(q[2]["uri"], "https://radio.example/live?token=theirs", "{q}");
    assert!(!q.to_string().contains("s3cret"), "{q}");
}

/// A saved queue whose only sign-ins are another host's `/v1/` link and a token this node never
/// made: the foreign link is left as it is, the node's is cleaned, and no session ends.
#[tokio::test]
async fn a_foreign_or_unknown_token_in_a_saved_queue_ends_no_session() {
    let dir = tempfile::tempdir().unwrap();
    let s = state(dir.path(), Some("s3cret")).await;
    let key = "queue.somewhere";
    let saved = serde_json::json!({
        "items": [
            {"id": 1, "uri": "https://other-node.example/v1/library/tracks/1/file?token=pmo_theirs"},
            {"id": 2, "uri": "track:2", "image": "/v1/library/albums/2/cover?token=pmo_not-ours"}
        ],
        "repeat": "off", "radio": false, "shuffle": false, "order": []
    });
    let browser = serde_json::json!([{"hash": blake3::hash(b"pmo_browser").to_hex().to_string(), "key": "owner", "created_at": 1}]).to_string();
    {
        let lib = s.library.lock().unwrap();
        lib.set_setting(key, &saved.to_string()).unwrap();
        lib.set_setting("accounts.owner_sessions", &browser).unwrap();
    }
    let cleaned = accounts::clean_saved_queues(&s).unwrap();
    assert_eq!(
        cleaned,
        accounts::CleanedQueues {
            items: 1,
            ..Default::default()
        }
    );
    let lib = s.library.lock().unwrap();
    assert_eq!(
        lib.get_setting("accounts.owner_sessions").unwrap().unwrap(),
        browser,
        "the session stays"
    );
    let json = lib.get_setting(key).unwrap().unwrap();
    assert!(
        json.contains("other-node.example/v1/library/tracks/1/file?token=pmo_theirs"),
        "{json}"
    );
    assert!(!json.contains("pmo_not-ours"), "{json}");
}

/// The second look behind the queue's own: an answer to a listener that carries a node link
/// with a sign-in (however it got there, not through a queue) goes out without it; another
/// host's `/v1/` link keeps its token.
#[tokio::test]
async fn a_listeners_answer_never_carries_a_node_sign_in() {
    use axum::response::IntoResponse;
    let me = ["http://127.0.0.1:7788".to_string()];
    let answer = Json(serde_json::json!({
        "now_playing": {"item": {"image": "/v1/library/albums/1/cover?token=pmo_abc"}},
        "queue": [{"uri": "http://127.0.0.1:7788/v1/library/tracks/1/file?a=1&token=s3cret"},
                  {"uri": "https://other.example/v1/x?token=theirs"},
                  {"uri": "http://localhost:7788/v1/library/tracks/2/file?token=s3cret"}]
    }))
    .into_response();
    let mine = |t: &str| t == "s3cret";
    let resp = accounts::listener_response(answer, &[], &me, &mine).await;
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains("pmo_abc") && !text.contains("s3cret"), "{text}");
    assert!(text.contains("/v1/library/albums/1/cover\""), "{text}");
    assert!(text.contains("/v1/library/tracks/1/file?a=1\""), "{text}");
    assert!(text.contains("https://other.example/v1/x?token=theirs"), "{text}");
    assert!(
        text.contains("\"http://localhost:7788/v1/library/tracks/2/file\""),
        "the node's own sign-in under another name: {text}"
    );
}

/// A cover link made under another of the node's names (the page opened at `localhost` while
/// `public_url` says the LAN address, a tunnel name) loses its sign-in like one under
/// `public_url`: on the way in, by the address the request came to and by the sign-in being the
/// node's own; a link on another host with a token that isn't the node's stays as it is.
#[tokio::test]
async fn a_cover_link_under_another_of_the_nodes_names_loses_its_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let s = state(dir.path(), Some("s3cret")).await;
    let app = router(s.clone());
    let (_, zones) = call_as(&app, "s3cret", "/v1/zones").await;
    let zid = zones[0]["id"].as_str().unwrap().to_string();
    assert_eq!(s.node_origins(), ["http://127.0.0.1:7788"]);
    let from_localhost = |method: &'static str, path: String, body: serde_json::Value| {
        let app = app.clone();
        async move {
            let req = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost:7788")
                .header("authorization", "Bearer s3cret")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            app.oneshot(req).await.unwrap().status()
        }
    };
    let items = serde_json::json!([
        {"uri": "https://radio.example/a", "title": "A", "image": "http://localhost:7788/v1/library/albums/1/cover?token=not-known-here"},
        {"uri": "https://radio.example/b", "title": "B", "image": "https://node.example.ts.net/v1/library/albums/1/cover?token=s3cret"},
        {"uri": "https://radio.example/c", "title": "C", "image": "https://other-node.example/v1/library/albums/1/cover?token=theirs"}
    ]);
    let st = from_localhost(
        "POST",
        format!("/v1/zones/{zid}/queue"),
        serde_json::json!({"items": items, "play": false}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, q) = call_as(&app, "s3cret", &format!("/v1/zones/{zid}/queue")).await;
    assert_eq!(
        q[0]["image"], "http://localhost:7788/v1/library/albums/1/cover",
        "the address the request came to: {q}"
    );
    assert_eq!(
        q[1]["image"], "https://node.example.ts.net/v1/library/albums/1/cover",
        "the node's own sign-in under any name: {q}"
    );
    assert_eq!(
        q[2]["image"], "https://other-node.example/v1/library/albums/1/cover?token=theirs",
        "{q}"
    );
    // The same on the way in through a move from the browser
    let st = from_localhost("POST", format!("/v1/zones/{zid}/load"), serde_json::json!({"items": items, "index": null, "position_ms": 0, "transport": "stopped", "repeat": "off", "radio": false})).await;
    assert_eq!(st, StatusCode::OK);
    let (_, q) = call_as(&app, "s3cret", &format!("/v1/zones/{zid}/queue")).await;
    assert_eq!(
        q[0]["image"], "http://localhost:7788/v1/library/albums/1/cover",
        "{q}"
    );
    assert_eq!(
        q[1]["image"], "https://node.example.ts.net/v1/library/albums/1/cover",
        "{q}"
    );
    assert_eq!(
        q[2]["image"], "https://other-node.example/v1/library/albums/1/cover?token=theirs",
        "{q}"
    );
}

/// The API token found in a saved queue can only be replaced by hand: Settings and Home's first
/// steps say so from then on, and Make a new token clears it.
#[tokio::test]
async fn an_exposed_api_token_is_said_until_it_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let s = state(dir.path(), Some("s3cret")).await;
    s.set_token_file(dir.path().join("api-token"));
    let app = router(s.clone());
    let (_, ns) = call_as(&app, "s3cret", "/v1/node/settings").await;
    assert!(ns.get("api_token_exposed_at").is_none(), "{ns}");
    s.library
        .lock()
        .unwrap()
        .set_setting(
            "queue.somewhere",
            &serde_json::json!({"items": [{"id": 1, "uri": "track:1", "image": "/v1/library/albums/1/cover?token=s3cret"}],
                "repeat": "off", "radio": false, "shuffle": false, "order": []})
            .to_string(),
        )
        .unwrap();
    assert!(accounts::clean_saved_queues(&s).unwrap().api_token_found);
    let (_, ns) = call_as(&app, "s3cret", "/v1/node/settings").await;
    assert!(ns["api_token_exposed_at"].as_i64().is_some_and(|t| t > 0), "{ns}");
    let (_, fs) = call_as(&app, "s3cret", "/v1/me/first-steps").await;
    assert_eq!(fs["steps"][0]["id"], "token", "{fs}");
    assert_eq!(fs["show"], true, "{fs}");
    // A new token: said no more
    let (st, t) = as_token(&app, "s3cret", "POST", "/v1/node/token", None).await;
    assert_eq!(st, StatusCode::OK, "{t}");
    let new: serde_json::Value = serde_json::from_str(&t).unwrap();
    let new = new["token"].as_str().unwrap().to_string();
    let (_, ns) = call_as(&app, &new, "/v1/node/settings").await;
    assert!(ns.get("api_token_exposed_at").is_none(), "{ns}");
    let (_, fs) = call_as(&app, &new, "/v1/me/first-steps").await;
    assert!(
        !fs["steps"].as_array().unwrap().iter().any(|x| x["id"] == "token"),
        "{fs}"
    );
}
