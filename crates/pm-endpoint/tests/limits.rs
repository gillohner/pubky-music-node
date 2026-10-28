//! A peer cannot hold the endpoint: a connection that never says Hello, or goes quiet before
//! opening an output, is closed; sessions beyond the cap are refused and the endpoint takes new
//! ones once others end (security review 2026-10-27, #6). Only the node with the player's key
//! gets a session: a missing or wrong key, or an old protocol, is refused with a reason, and
//! such connections never hold a session slot or outlast the Hello deadline.

use pm_endpoint::{Endpoint, Limits};
use pm_stream::{read_frame, write_frame, Frame, Incoming, Key};
use pm_zones::outputs::NullOutput;
use pm_zones::OutputRegistry;
use std::io::{BufReader, BufWriter, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn start(limits: Limits) -> (SocketAddr, Key) {
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let key = Key::random();
    let ep = Endpoint::with_limits("Test", outputs, key.clone(), limits);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || ep.serve(l).unwrap());
    (addr, key)
}

fn connect(addr: SocketAddr) -> (BufReader<TcpStream>, BufWriter<TcpStream>) {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    (BufReader::new(s.try_clone().unwrap()), BufWriter::new(s))
}

fn hello_as(
    addr: SocketAddr,
    version: u32,
    key: &Key,
) -> (Frame, BufReader<TcpStream>, BufWriter<TcpStream>) {
    let (mut r, mut w) = connect(addr);
    write_frame(
        &mut w,
        &Frame::Hello {
            client: "test".into(),
            version,
            key: key.clone(),
        },
    )
    .unwrap();
    let f = match read_frame(&mut r).unwrap() {
        Incoming::Control(f) => f,
        Incoming::Audio(_) => panic!("audio"),
    };
    (f, r, w)
}

fn hello(addr: SocketAddr, key: &Key) -> (Frame, BufReader<TcpStream>, BufWriter<TcpStream>) {
    hello_as(addr, pm_stream::PROTOCOL_VERSION, key)
}

fn refusal(f: Frame) -> String {
    match f {
        Frame::Error { message } => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn silent_peers_are_closed() {
    let short = Duration::from_millis(300);
    let (addr, key) = start(Limits {
        max_sessions: 4,
        max_pending: 4,
        hello: short,
        idle: short,
    });
    // Never says Hello
    let (mut r, _w) = connect(addr);
    let t = Instant::now();
    assert!(read_frame(&mut r).is_err(), "closed");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    // Says Hello, then nothing
    let (f, mut r, _w) = hello(addr, &key);
    assert!(matches!(f, Frame::Welcome { .. }));
    let t = Instant::now();
    assert!(read_frame(&mut r).is_err(), "closed");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}

#[test]
fn a_trickled_hello_cannot_stretch_the_deadline() {
    let (addr, _) = start(Limits {
        hello: Duration::from_millis(500),
        ..Limits::default()
    });
    let (mut r, mut w) = connect(addr);
    let t = Instant::now();
    let trickle = std::thread::spawn(move || {
        // A frame head claiming a large Hello, then a byte every 100 ms
        let _ = w.write_all(&[1, 0, 0, 1, 0]);
        let _ = w.flush();
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            if w.write_all(b" ").and_then(|()| w.flush()).is_err() {
                break;
            }
        }
    });
    assert!(read_frame(&mut r).is_err(), "closed");
    assert!(t.elapsed() < Duration::from_secs(2), "held for {:?}", t.elapsed());
    trickle.join().unwrap();
}

#[test]
fn missing_wrong_and_old_hellos_are_refused() {
    let (addr, key) = start(Limits::default());
    let m = refusal(hello_as(addr, 1, &Key::default()).0);
    assert_eq!(m, "endpoint speaks protocol 2, this node 1; update the node");
    let m = refusal(hello_as(addr, 3, &key).0);
    assert!(
        m.contains("protocol 2, this node 3; update the Pubky Music app"),
        "{m}"
    );
    assert!(refusal(hello(addr, &Key::default()).0).starts_with("no key"));
    assert!(refusal(hello(addr, &Key::random()).0).starts_with("wrong key"));
    // Not even a Hello
    let (mut r, mut w) = connect(addr);
    write_frame(&mut w, &Frame::Close).unwrap();
    let Incoming::Control(f) = read_frame(&mut r).unwrap() else {
        panic!()
    };
    assert_eq!(refusal(f), "expected Hello");
    match hello(addr, &key).0 {
        Frame::Welcome { version, outputs, .. } => {
            assert_eq!(version, pm_stream::PROTOCOL_VERSION);
            assert_eq!(outputs.len(), 1);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn bad_keys_never_take_a_session_slot() {
    let short = Duration::from_millis(400);
    let (addr, key) = start(Limits {
        max_sessions: 1,
        max_pending: 3,
        hello: short,
        idle: Duration::from_secs(30),
    });
    // Many wrong keys, one after another: each is turned away, none counts as a session
    for _ in 0..20 {
        assert!(refusal(hello(addr, &Key::random()).0).starts_with("wrong key"));
    }
    // Silent connections fill only the waiting room, and leave by the Hello deadline
    let silent: Vec<_> = (0..3).map(|_| connect(addr)).collect();
    std::thread::sleep(Duration::from_millis(100));
    let (mut r, _w) = connect(addr);
    let Incoming::Control(f) = read_frame(&mut r).unwrap() else {
        panic!()
    };
    assert_eq!(refusal(f), "too many connections");
    for (mut r, _w) in silent {
        assert!(read_frame(&mut r).is_err(), "closed by the deadline");
    }
    // The one session slot is still free for the node
    let (f, _r, _w) = hello(addr, &key);
    assert!(matches!(f, Frame::Welcome { .. }), "{f:?}");
    assert_eq!(refusal(hello(addr, &key).0), "too many sessions");
}

#[test]
fn sessions_beyond_the_cap_are_refused() {
    let long = Duration::from_secs(30);
    let (addr, key) = start(Limits {
        max_sessions: 2,
        max_pending: 4,
        hello: long,
        idle: long,
    });
    let a = hello(addr, &key);
    let _b = hello(addr, &key);
    assert!(refusal(hello(addr, &key).0).contains("too many sessions"));
    // One ends: a new session is taken
    drop(a);
    let t = Instant::now();
    loop {
        if let Frame::Welcome { .. } = hello(addr, &key).0 {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "the freed slot was never reused"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
