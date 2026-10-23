//! A remote library root (WebDAV) scanned through `Scanner::scan_remote` with ranged readers.

use pm_library::scan::{RemoteEntry, Scanner};
use pm_library::tags::RemoteRead;
use pm_library::Library;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// A "remote" file served from memory, counting the bytes it was asked for.
struct Mem(Cursor<Vec<u8>>);

impl std::io::Read for Mem {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}
impl std::io::Seek for Mem {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}
impl RemoteRead for Mem {
    fn byte_len(&self) -> Option<u64> {
        Some(self.0.get_ref().len() as u64)
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pm-audio/tests/fixtures")
            .join(name),
    )
    .unwrap()
}

#[test]
fn a_remote_root_is_scanned_rescanned_and_pruned() {
    let lib = Library::open_in_memory().unwrap();
    let scanner = Scanner::new(&lib);
    let root = PathBuf::from("webdav://share1");
    let files: Vec<(PathBuf, Vec<u8>)> = vec![
        (root.join("Album/01 Mono.flac"), fixture("60_mono_audio.flac")),
        (root.join("Album/02 Wasted.flac"), fixture("14_wasted_bits.flac")),
        (root.join("notes.txt"), b"not audio".to_vec()),
    ];
    let listing = |files: &[(PathBuf, Vec<u8>)]| -> Vec<RemoteEntry> {
        files
            .iter()
            .map(|(p, b)| RemoteEntry {
                path: p.clone(),
                size: b.len() as u64,
                mtime: 1_700_000_000,
            })
            .collect()
    };
    let open = |files: Vec<(PathBuf, Vec<u8>)>| {
        move |e: &RemoteEntry| -> std::io::Result<Box<dyn RemoteRead>> {
            let (_, b) = files
                .iter()
                .find(|(p, _)| *p == e.path)
                .ok_or(std::io::ErrorKind::NotFound)?;
            Ok(Box::new(Mem(Cursor::new(b.clone()))))
        }
    };
    let r = scanner
        .scan_remote(&root, listing(&files), &open(files.clone()))
        .unwrap();
    assert_eq!(
        (r.files_seen, r.files_added, r.errors.len()),
        (2, 2, 0),
        "{:?}",
        r.errors
    );
    let t = lib
        .file_by_path(&root.join("Album/01 Mono.flac"))
        .unwrap()
        .expect("indexed");
    let track = lib.get_track(t.track_id).unwrap().unwrap();
    assert_eq!(track.file.container, "flac");
    assert!(track.file.duration_ms.unwrap() > 1000);
    // Again: nothing to read
    let r = scanner
        .scan_remote(&root, listing(&files), &open(files.clone()))
        .unwrap();
    assert_eq!((r.files_unchanged, r.files_added), (2, 0));
    // One file gone from the share
    let left = vec![files[0].clone()];
    let r = scanner
        .scan_remote(&root, listing(&left), &open(left.clone()))
        .unwrap();
    assert_eq!(r.files_removed, 1);
    assert!(
        lib.file_by_path(&root.join("Album/02 Wasted.flac"))
            .unwrap()
            .unwrap()
            .missing
    );
}
