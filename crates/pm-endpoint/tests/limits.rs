//! A peer cannot hold the endpoint: a connection that never says Hello, or goes quiet before
//! opening an output, is closed; sessions beyond the cap are refused and the endpoint takes new
//! ones once others end (security review 2026-10-27, #6).

use pm_endpoint::{Endpoint, Limits};
use pm_stream::{read_frame, write_frame, Frame, Incoming};
use pm_zones::outputs::NullOutput;
use pm_zones::OutputRegistry;
use std::io::{BufReader, BufWriter};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn start(limits: Limits) -> SocketAddr {
    let outputs = OutputRegistry::new();
    outputs.register(Arc::new(NullOutput::new()));
    let ep = Endpoint::with_limits("Test", outputs, limits);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || ep.serve(l).unwrap());
    addr
}

fn connect(addr: SocketAddr) -> (BufReader<TcpStream>, BufWriter<TcpStream>) {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    (BufReader::new(s.try_clone().unwrap()), BufWriter::new(s))
}

fn hello(r: &mut BufReader<TcpStream>, w: &mut BufWriter<TcpStream>) -> Frame {
    write_frame(
        w,
        &Frame::Hello {
            client: "test".into(),
            version: pm_stream::PROTOCOL_VERSION,
        },
    )
    .unwrap();
    match read_frame(r).unwrap() {
        Incoming::Control(f) => f,
        Incoming::Audio(_) => panic!("audio"),
    }
}

#[test]
fn silent_peers_are_closed() {
    let short = Duration::from_millis(300);
    let addr = start(Limits {
        max_sessions: 4,
        hello: short,
        idle: short,
    });
    // Never says Hello
    let (mut r, _w) = connect(addr);
    let t = Instant::now();
    assert!(read_frame(&mut r).is_err(), "closed");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    // Says Hello, then nothing
    let (mut r, mut w) = connect(addr);
    assert!(matches!(hello(&mut r, &mut w), Frame::Welcome { .. }));
    let t = Instant::now();
    assert!(read_frame(&mut r).is_err(), "closed");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}

#[test]
fn sessions_beyond_the_cap_are_refused() {
    let long = Duration::from_secs(30);
    let addr = start(Limits {
        max_sessions: 2,
        hello: long,
        idle: long,
    });
    let a = connect(addr);
    let _b = connect(addr);
    std::thread::sleep(Duration::from_millis(100));
    let (mut r, _w) = connect(addr);
    match read_frame(&mut r) {
        Ok(Incoming::Control(Frame::Error { message })) => assert!(message.contains("too many"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    // One ends: a new session is taken
    drop(a);
    let t = Instant::now();
    loop {
        let (mut r, mut w) = connect(addr);
        write_frame(
            &mut w,
            &Frame::Hello {
                client: "test".into(),
                version: pm_stream::PROTOCOL_VERSION,
            },
        )
        .unwrap();
        if let Ok(Incoming::Control(Frame::Welcome { .. })) = read_frame(&mut r) {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "the freed slot was never reused"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
