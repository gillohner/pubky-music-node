//! Settings in four groups (STEWARD-SETTINGS-GROUPS.md): the person's own preferences
//! (`/v1/me/prefs`, "You"), the node's index of the groups (`/v1/node`), and "This homeserver"
//! answering 404 without a bundled one.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pm_api::{router, NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_library::Library;
use pm_zones::OutputRegistry;
use serde_json::{json, Value};
use tower::ServiceExt;

const OWNER: &str = "owner-token";
const KEY: &str = "o3ftkuxjd9k6zgnc4hhbhbg5jusubr4zqyb1dtzi8hhm3hf5wbmy";

async fn call(
    app: &axum::Router,
    token: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
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
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn node(dir: &std::path::Path) -> std::sync::Arc<NodeState> {
    let db = dir.join("lib.sqlite");
    NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![],
            art_dir: dir.join("art"),
            mirror_dir: dir.join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: Some(OWNER.into()),
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
    )
}

#[tokio::test]
async fn preferences_override_the_nodes_defaults_and_say_which_applies() {
    let dir = tempfile::tempdir().unwrap();
    let s = node(dir.path());
    let app = router(s.clone());

    // Without a bundled homeserver: "This homeserver" is a 404, and the index leaves it out
    assert_eq!(
        call(&app, OWNER, "GET", "/v1/homeserver/bundled", None).await.0,
        StatusCode::NOT_FOUND
    );
    let (st, ix) = call(&app, OWNER, "GET", "/v1/node", None).await;
    assert_eq!(st, StatusCode::OK, "{ix}");
    let ids: Vec<&str> = ix["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["you", "device", "node"]);
    // Every route the index names is one the node answers (not 404 for every method)
    for g in ix["groups"].as_array().unwrap() {
        for sec in g["sections"].as_array().unwrap() {
            for r in sec["routes"].as_array().unwrap() {
                let r = r.as_str().unwrap().replace("{id}", "1");
                let mut found = false;
                for m in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
                    // A route nobody serves answers 404 with no body; a handler's 404 (no such
                    // track) carries the node's JSON error
                    let (st, body) = call(&app, OWNER, m, &r, None).await;
                    if (st != StatusCode::NOT_FOUND && st != StatusCode::METHOD_NOT_ALLOWED)
                        || !body.is_null()
                    {
                        found = true;
                        break;
                    }
                }
                assert!(found, "/v1/node lists {r}, which nothing answers");
            }
        }
    }
    // The tunnel answers at its new place as at the old one
    let old = call(&app, OWNER, "GET", "/v1/remote/tunnel", None).await;
    let new = call(&app, OWNER, "GET", "/v1/homeserver/bundled/tunnel", None).await;
    assert_eq!(old, new);

    // Nothing set: the node's defaults apply
    let (st, p) = call(&app, OWNER, "GET", "/v1/me/prefs", None).await;
    assert_eq!(st, StatusCode::OK, "{p}");
    assert_eq!(p["saved_on"], "node", "not signed in with Pubky");
    assert_eq!(p["home_layout"]["applies"], "node default");
    assert_eq!(
        p["appearance"]["colour_from"],
        json!({"value": "source", "applies": "node default"})
    );
    assert_eq!(p["quality"], json!({}));

    // The node's Home layout is everyone's default
    call(
        &app,
        OWNER,
        "PUT",
        "/v1/home/layout",
        Some(json!({"order": ["recent"], "hidden": []})),
    )
    .await;
    let (_, p) = call(&app, OWNER, "GET", "/v1/me/prefs", None).await;
    assert_eq!(p["home_layout"]["value"]["order"], json!(["recent"]));

    // The owner's own choices win, and say so
    let (st, p) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/me/prefs",
        Some(
            json!({"home_layout": {"order": ["favourites"], "hidden": ["radio"]},
                    "quality": {"tidal": "max"}, "appearance": {"colour_from": "art"}}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{p}");
    assert_eq!(p["home_layout"]["applies"], "yours");
    assert_eq!(p["quality"]["tidal"], json!({"value": "max", "applies": "yours"}));
    assert_eq!(
        p["appearance"]["colour_from"],
        json!({"value": "art", "applies": "yours"})
    );
    // /v1/home/layout stays the node's (it round-trips with its PUT); prefs say what applies
    let (_, l) = call(&app, OWNER, "GET", "/v1/home/layout", None).await;
    assert_eq!(l["order"], json!(["recent"]));
    let (_, p2) = call(&app, OWNER, "GET", "/v1/me/prefs", None).await;
    assert_eq!(p2["home_layout"]["value"]["order"], json!(["favourites"]));
    // A member left out stays; null drops the override
    let (_, p) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/me/prefs",
        Some(json!({"home_layout": null, "quality": {"tidal": null}})),
    )
    .await;
    assert_eq!(p["home_layout"]["applies"], "node default");
    assert_eq!(p["quality"], json!({}));
    assert_eq!(p["appearance"]["colour_from"]["value"], "art");
    let (_, l) = call(&app, OWNER, "GET", "/v1/home/layout", None).await;
    assert_eq!(l["order"], json!(["recent"]));

    // Refused: a colour source that doesn't exist, a malformed quality, a bad layout
    for bad in [
        json!({"appearance": {"colour_from": "rainbow"}}),
        json!({"quality": {"tidal": "MAX!"}}),
        json!({"quality": {"Not a source": "max"}}),
        json!({"home_layout": {"order": ["Bad Row"], "hidden": []}}),
        json!({"apperance": {"colour_from": "art"}}),
    ] {
        let (st, v) = call(&app, OWNER, "PUT", "/v1/me/prefs", Some(bad.clone())).await;
        assert!(
            st == StatusCode::BAD_REQUEST || st == StatusCode::UNPROCESSABLE_ENTITY,
            "{bad} → {st} {v}"
        );
    }

    // A listener has their own, kept on the node; the owner's don't change
    let roster = json!({"people": [{"key": KEY, "access": "music", "music_enabled": true, "added_at": 0}]});
    s.library
        .lock()
        .unwrap()
        .set_setting("people", &roster.to_string())
        .unwrap();
    let listener = pm_api::accounts::mint(&s, KEY).unwrap();
    let (st, p) = call(
        &app,
        &listener,
        "PUT",
        "/v1/me/prefs",
        Some(json!({"appearance": {"colour_from": "source"}})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{p}");
    assert_eq!(p["saved_on"], "node");
    assert_eq!(
        p["appearance"]["colour_from"],
        json!({"value": "source", "applies": "yours"})
    );
    let (_, p) = call(&app, OWNER, "GET", "/v1/me/prefs", None).await;
    assert_eq!(p["appearance"]["colour_from"]["value"], "art");
    // The index of the node's settings is the owner's
    assert_eq!(
        call(&app, &listener, "GET", "/v1/node", None).await.0,
        StatusCode::FORBIDDEN
    );

    // Service apps: one list, never a secret
    let (st, apps) = call(&app, OWNER, "GET", "/v1/node/apps", None).await;
    assert_eq!(st, StatusCode::OK, "{apps}");
    let ids: Vec<&str> = apps
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    for want in [
        "spotify",
        "tidal",
        "tidal-full",
        "qobuz",
        "soundcloud",
        "youtube",
        "lastfm",
        "jamendo",
        "acoustid",
        "google-drive",
        "onedrive",
    ] {
        assert!(ids.contains(&want), "{want} missing from {ids:?}");
    }
    let find = |a: &Value, id: &str| {
        a.as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(find(&apps, "spotify")["configured"], false);
    assert!(find(&apps, "spotify")["get_one"]
        .as_str()
        .unwrap()
        .starts_with("https://"));
    let secret = "0123456789abcdef0123456789abcdef";
    let (st, v) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/sources/spotify/app",
        Some(json!({"client_id": "fedcba9876543210fedcba9876543210"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = call(
        &app,
        OWNER,
        "PUT",
        "/v1/lastfm",
        Some(json!({"api_key": "aaaabbbbccccddddaaaabbbbccccdddd", "api_secret": secret})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, apps) = call(&app, OWNER, "GET", "/v1/node/apps", None).await;
    assert_eq!(find(&apps, "spotify")["configured"], true);
    assert_eq!(find(&apps, "spotify")["from"], "settings");
    assert_eq!(find(&apps, "lastfm")["configured"], true);
    assert_eq!(find(&apps, "lastfm")["from"], "settings");
    assert!(!apps.to_string().contains(secret), "a secret in {apps}");
    assert_eq!(
        call(&app, &listener, "GET", "/v1/node/apps", None).await.0,
        StatusCode::FORBIDDEN
    );
}
