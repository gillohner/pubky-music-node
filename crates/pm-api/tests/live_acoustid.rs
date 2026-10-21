//! Live check of identification against the real AcoustID service. Ignored
//! by default (needs the network, an application key and audio files):
//!
//! ```sh
//! PM_ACOUSTID_KEY=… PM_LIVE_AUDIO=/path/a.mp3:/path/b.flac \
//!   cargo test -p pm-api --test live_acoustid -- --ignored --nocapture
//! ```
//!
//! Each file is fingerprinted in-process (`pm_audio::fingerprint`), looked up
//! with the node's client, and decided with the node's rules (no folder).

use pm_library::identify::{decide, AcoustIdClient, AcoustIdConfig, Decision, FileFacts};

#[tokio::test]
#[ignore]
async fn live_acoustid_identifies_real_files() {
    let key = std::env::var("PM_ACOUSTID_KEY").expect("PM_ACOUSTID_KEY");
    let files = std::env::var("PM_LIVE_AUDIO").expect("PM_LIVE_AUDIO (paths separated by ':')");
    let client = AcoustIdClient::new(AcoustIdConfig::new(
        key,
        "pubky-music-node/test (https://github.com/gillohner/pubky-music-node)",
    ))
    .unwrap();
    let mut identified = 0;
    let total = files.split(':').count();
    for path in files.split(':') {
        let p = path.to_string();
        let fp = tokio::task::spawn_blocking(move || pm_audio::fingerprint::fingerprint_file(&p))
            .await
            .unwrap()
            .expect("fingerprint");
        let (_, results) = client
            .lookup(&fp.fingerprint, fp.duration_secs)
            .await
            .expect("lookup");
        let facts = FileFacts {
            duration_secs: Some(fp.duration_secs as f64),
            ..Default::default()
        };
        let best = results.iter().map(|r| r.score).fold(0.0_f32, f32::max);
        let d = decide(&results, &facts, 0, None);
        let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy();
        match &d {
            Decision::Identified {
                recording,
                confidence,
                ..
            } => {
                identified += 1;
                let title = results
                    .iter()
                    .flat_map(|r| &r.recordings)
                    .find(|r| r.id == recording.to_string())
                    .and_then(|r| r.title.clone())
                    .unwrap_or_default();
                println!(
                    "{name}: identified as \"{title}\" https://musicbrainz.org/recording/{recording} ({})",
                    confidence.label()
                );
            }
            Decision::Suggest(c) => println!("{name}: {} suggestion(s), best score {best:.2}", c.len()),
            Decision::Unknown if results.is_empty() => println!("{name}: unknown to AcoustID"),
            Decision::Unknown => println!(
                "{name}: AcoustID knows the audio (score {best:.2}) but links no MusicBrainz recording yet"
            ),
        }
    }
    println!("{identified} of {total} identified");
}
