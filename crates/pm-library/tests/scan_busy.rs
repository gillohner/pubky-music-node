//! A scan while another connection writes (a second scan, the API): no file is skipped with
//! "database is locked".

use pm_library::{Library, Scanner};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ietf-60-mono-audio.flac")
}

#[test]
fn a_scan_loses_no_file_while_another_connection_writes() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir_all(&music).unwrap();
    let flac = std::fs::read(fixture()).unwrap();
    const FILES: usize = 300;
    for i in 0..FILES {
        std::fs::write(music.join(format!("{i:02} song.flac")), &flac).unwrap();
    }
    let db = dir.path().join("lib.sqlite");
    let lib = Library::open(&db).unwrap();

    // Another writer commits small transactions all the time, as a second scan or the API does
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (db, stop) = (db.clone(), stop.clone());
        std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.busy_handler(Some(|t| {
                std::thread::sleep(std::time::Duration::from_millis(1));
                t < 30_000
            }))
            .unwrap();
            conn.execute_batch("CREATE TABLE IF NOT EXISTS other_writer (n INTEGER)")
                .unwrap();
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                conn.execute_batch("BEGIN IMMEDIATE").unwrap();
                conn.execute("INSERT INTO other_writer (n) VALUES (?1)", [n])
                    .unwrap();
                std::thread::sleep(std::time::Duration::from_millis(3));
                conn.execute_batch("COMMIT").unwrap();
                n += 1;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            n
        })
    };
    // The writer is busy before the scan starts, and stays busy until it ends
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let probe = rusqlite::Connection::open(&db).unwrap();
    while probe
        .query_row("SELECT count(*) FROM other_writer", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
        < 3
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the other writer never wrote"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let t0 = std::time::Instant::now();
    let report = Scanner::new(&lib)
        .scan(&music)
        .unwrap_or_else(|e| panic!("after {:?}: {e:?}", t0.elapsed()));
    stop.store(true, Ordering::Relaxed);
    let writes = writer.join().unwrap();
    assert!(writes > 0);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.files_added as usize, FILES);
    assert_eq!(report.files_seen as usize, FILES);
}
