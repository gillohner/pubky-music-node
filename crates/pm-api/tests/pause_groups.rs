//! A node group of outputs on different protocols falls silent together on pause: every member
//! gets its own protocol's pause (MPD `pause 1`, BluOS `/Pause`), and its own play on resume.

use pm_model::AudioFormat;
use pm_zones::outputs::{GroupOutput, PullOutput};
use pm_zones::Output;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

type Log = Arc<Mutex<Vec<String>>>;

/// An MPD that logs its commands, gives the added stream id 42 and plays it.
fn fake_mpd(log: Log) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let log = log.clone();
            std::thread::spawn(move || {
                conn.write_all(b"OK MPD 0.24.0\n").unwrap();
                let mut r = BufReader::new(conn.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let cmd = line.trim_end().to_string();
                    let answer = match cmd.split_whitespace().next() {
                        Some("addid") => "Id: 42\nOK\n",
                        Some("status") => "state: play\nsongid: 42\nOK\n",
                        _ => "OK\n",
                    };
                    log.lock().unwrap().push(cmd);
                    let _ = conn.write_all(answer.as_bytes());
                }
            });
        }
    });
    port
}

/// A BluOS player that logs each call's path and answers 200.
fn fake_bluos(log: Log) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut r = BufReader::new(conn.try_clone().unwrap());
            let mut first = String::new();
            r.read_line(&mut first).unwrap();
            loop {
                let mut l = String::new();
                if r.read_line(&mut l).unwrap() == 0 || l == "\r\n" {
                    break;
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
            log.lock().unwrap().push(path);
            let body = "<state>stream</state>";
            let _ = conn.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    port
}

/// Pause on a group of an MPD server and a BluOS player: each gets its own pause, then its own
/// play (before, the group only stopped feeding them, and both played out their buffer).
#[tokio::test(flavor = "multi_thread")]
async fn a_mixed_group_pauses_each_member_with_its_own_protocol() {
    let (mpd_log, bluos_log) = (Log::default(), Log::default());
    let mpd = Arc::new(pm_out_mpd::MpdOutput::new(
        pm_out_mpd::Server::parse(&format!("127.0.0.1:{}", fake_mpd(mpd_log.clone()))).unwrap(),
        Arc::new(PullOutput::controlled("mpd")),
        "http://node/mpd.wav".into(),
    ));
    let bluos = Arc::new(pm_out_bluos::BluosOutput::new(
        pm_out_bluos::Player {
            id: "aa01".into(),
            name: "Den".into(),
            host: "127.0.0.1".into(),
            port: fake_bluos(bluos_log.clone()),
            model: None,
        },
        Arc::new(PullOutput::controlled("bluos")),
        "http://node/bluos.wav".into(),
    ));
    let group = GroupOutput::new("Everywhere", vec![mpd, bluos]).unwrap();
    let mut sink = group.open(AudioFormat::pcm(44_100, 16, 2)).await.unwrap();
    std::thread::spawn(move || {
        assert!(sink.pause().unwrap(), "both members hold their buffer");
        sink.resume().unwrap();
        drop(sink);
    })
    .join()
    .unwrap();
    let mpd_log = mpd_log.lock().unwrap().clone();
    assert_eq!(
        mpd_log,
        [
            "addid \"http://node/mpd.wav\"",
            "playid 42",
            "pause 1",
            "pause 0",
            "status",
            "stop",
            "deleteid 42",
        ]
    );
    let bluos_log = bluos_log.lock().unwrap().clone();
    assert_eq!(
        bluos_log,
        [
            "/Play?url=http%3A%2F%2Fnode%2Fbluos.wav",
            "/Pause",
            "/Play",
            "/Stop"
        ]
    );
}
