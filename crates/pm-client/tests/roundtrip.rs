//! The client against an in-process node API: scan, browse, enqueue, play,
//! events, and a track download.

use pm_api::types::{ApiEvent, TransportAction};
use pm_api::{NodeSettings, NodeState};
use pm_audio::PipelineConfig;
use pm_client::NodeClient;
use pm_library::Library;
use pm_zones::outputs::NullOutput;
use pm_zones::{OutputRegistry, Transport};
use std::sync::Arc;
use std::time::Duration;

fn write_wav(path: &std::path::Path, frames: u32) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..frames {
        w.write_sample((i % 100) as i16).unwrap();
        w.write_sample(-((i % 100) as i16)).unwrap();
    }
    w.finalize().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_wav(&music.join("song.wav"), 4410);
    let db = dir.path().join("lib.sqlite");
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let state = NodeState::new(
        NodeSettings {
            name: "t".into(),
            version: "0".into(),
            db_path: db.clone(),
            scan_folders: vec![music.clone()],
            art_dir: dir.path().join("art"),
            mirror_dir: dir.path().join("homeserver"),
            pipeline: PipelineConfig::default(),
            token: Some("tok".into()),
        },
        Library::open(&db).unwrap(),
        outputs,
    );
    state.zones_for_all_outputs();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(pm_api::serve_listener(state.clone(), listener));

    let bad = NodeClient::new(&format!("http://{addr}"), None).unwrap();
    let err = bad.info().await.unwrap_err();
    assert!(
        matches!(err, pm_client::ClientError::Api { status: 401, .. }),
        "{err}"
    );

    let c = NodeClient::new(&format!("http://{addr}"), Some("tok".into())).unwrap();
    let info = c.info().await.unwrap();
    assert_eq!(info.api, "v1");
    let mut events = c.events().await.unwrap();
    let first = events.next().await.unwrap().unwrap();
    assert!(matches!(first, ApiEvent::Snapshot { .. }));

    c.scan(vec![]).await.unwrap();
    for _ in 0..500 {
        if !state.is_scanning() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let tracks = c.tracks(None, 10).await.unwrap();
    assert_eq!(tracks.len(), 1);
    let id = tracks[0].id.unwrap().0;
    assert_eq!(c.search("song", 10).await.unwrap()[0].id.unwrap().0, id);
    assert_eq!(c.track(id).await.unwrap().title, "song");

    // Download is byte-identical to the file on the node.
    let dl = dir.path().join("dl.wav");
    let n = c.download_track(id, &dl).await.unwrap();
    assert_eq!(n, std::fs::metadata(music.join("song.wav")).unwrap().len());
    assert_eq!(
        std::fs::read(&dl).unwrap(),
        std::fs::read(music.join("song.wav")).unwrap()
    );

    let zones = c.zones().await.unwrap();
    let zid = zones[0].id.0.clone();
    c.play_tracks(&zid, vec![id]).await.unwrap();
    // The event stream reports the zone playing and then stopping.
    let mut saw_playing = false;
    let mut saw_stopped_after = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !saw_stopped_after {
        match tokio::time::timeout_at(deadline, events.next()).await {
            Ok(Some(Ok(ApiEvent::Zone { state }))) if state.id.0 == zid => {
                if state.transport == Transport::Playing {
                    saw_playing = true;
                } else if saw_playing && state.transport == Transport::Stopped {
                    saw_stopped_after = true;
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
    assert!(saw_playing && saw_stopped_after);
    let q = c.queue(&zid).await.unwrap();
    assert_eq!(q[0].title.as_deref(), Some("song"));
    let st = c.transport(&zid, TransportAction::Stop).await.unwrap();
    assert_eq!(st.transport, Transport::Stopped);
    c.clear_queue(&zid).await.unwrap();
    assert!(c.queue(&zid).await.unwrap().is_empty());
    let kitchen = c.create_zone("Kitchen", "null").await.unwrap();
    assert_eq!(kitchen.id.0, "kitchen");
    c.delete_zone("kitchen").await.unwrap();
    assert_eq!(c.zones().await.unwrap().len(), 1);
}
