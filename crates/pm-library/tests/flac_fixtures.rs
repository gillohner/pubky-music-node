//! Probe the IETF FLAC conformance fixtures (see tests/fixtures/README.md).

use pm_library::tags::probe_and_read;
use pm_model::SampleKind;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn mono_16bit_flac() {
    let (probe, tags) = probe_and_read(&fixture("ietf-60-mono-audio.flac")).unwrap();
    assert_eq!(probe.container, "flac");
    assert_eq!(probe.format.kind, SampleKind::Pcm);
    assert_eq!(probe.format.channels, 1);
    assert_eq!(probe.format.bits, 16);
    assert_eq!(probe.format.rate_hz, 44_100);
    assert!(
        probe.duration_ms.unwrap_or(0) > 0,
        "duration {:?}",
        probe.duration_ms
    );
    // Conformance files carry no MusicBrainz identifiers.
    assert!(tags.recording_mbid.is_none());
    assert!(tags.isrc.is_none());
    eprintln!("tags: {tags:?}");
}

#[test]
fn mono_24bit_flac() {
    let (probe, tags) = probe_and_read(&fixture("ietf-63-predictor-overflow-24bit.flac")).unwrap();
    assert_eq!(probe.container, "flac");
    assert_eq!(probe.format.channels, 1);
    assert_eq!(probe.format.bits, 24);
    assert!(probe.duration_ms.unwrap_or(0) > 0);
    assert!(tags.recording_mbid.is_none());
    eprintln!("tags: {tags:?}");
}

/// Build a FLAC with a VORBIS_COMMENT block by splicing one into the mono
/// fixture (which has an empty comment block), then check tag reading.
#[test]
fn flac_vorbis_comments_are_read() {
    use pm_model::{Isrc, Mbid};
    let src = std::fs::read(fixture("ietf-60-mono-audio.flac")).unwrap();
    assert_eq!(&src[..4], b"fLaC");
    let comments = [
        "TITLE=Spliced Song",
        "ARTIST=Artist One",
        "ARTIST=Artist Two",
        "ALBUM=Spliced Album",
        "ALBUMARTIST=Various",
        "TRACKNUMBER=3",
        "DISCNUMBER=2",
        "DISCTOTAL=2",
        "DATE=2021-11-11",
        "MUSICBRAINZ_TRACKID=b1a9c0e9-d987-4042-ae91-78d6a3267d69",
        "MUSICBRAINZ_ALBUMID=0383dadf-2a4e-4d10-a46a-e9e041da8eb3",
        "MUSICBRAINZ_RELEASEGROUPID=0383dadf-2a4e-4d10-a46a-e9e041da8eb3",
        "MUSICBRAINZ_ARTISTID=0383dadf-2a4e-4d10-a46a-e9e041da8eb3",
        "ISRC=USQX91300108",
        "BARCODE=888837168618",
        "ACOUSTID_ID=acoustid-abc",
        "REPLAYGAIN_TRACK_GAIN=-3.50 dB",
    ];
    let mut body = Vec::new();
    let vendor = b"pm-library test";
    body.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    body.extend_from_slice(vendor);
    body.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        body.extend_from_slice(&(c.len() as u32).to_le_bytes());
        body.extend_from_slice(c.as_bytes());
    }
    // Walk the original blocks; replace the (empty) VORBIS_COMMENT block.
    let mut out = src[..4].to_vec();
    let mut i = 4;
    loop {
        let h = src[i];
        let last = h & 0x80 != 0;
        let ty = h & 0x7f;
        let len = u32::from_be_bytes([0, src[i + 1], src[i + 2], src[i + 3]]) as usize;
        if ty == 4 {
            out.push((if last { 0x80 } else { 0 }) | 4);
            out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
            out.extend_from_slice(&body);
        } else {
            out.extend_from_slice(&src[i..i + 4 + len]);
        }
        i += 4 + len;
        if last {
            break;
        }
    }
    out.extend_from_slice(&src[i..]);
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("spliced.flac");
    std::fs::write(&p, out).unwrap();

    let (probe, tags) = probe_and_read(&p).unwrap();
    assert_eq!(probe.container, "flac");
    assert_eq!(tags.title.as_deref(), Some("Spliced Song"));
    assert_eq!(
        tags.artists,
        vec!["Artist One".to_string(), "Artist Two".to_string()]
    );
    assert_eq!(tags.album.as_deref(), Some("Spliced Album"));
    assert_eq!(tags.album_artist.as_deref(), Some("Various"));
    assert_eq!(tags.track_no, Some(3));
    assert_eq!(tags.disc_no, Some(2));
    assert_eq!(tags.total_discs, Some(2));
    assert_eq!(tags.year, Some(2021));
    assert_eq!(
        tags.recording_mbid,
        Some(Mbid::parse("b1a9c0e9-d987-4042-ae91-78d6a3267d69").unwrap())
    );
    assert_eq!(
        tags.release_mbid,
        Some(Mbid::parse("0383dadf-2a4e-4d10-a46a-e9e041da8eb3").unwrap())
    );
    assert!(tags.release_group_mbid.is_some());
    assert_eq!(tags.artist_mbids.len(), 1);
    assert_eq!(tags.isrc, Some(Isrc::parse("USQX91300108").unwrap()));
    assert!(tags.upc.is_some());
    assert_eq!(tags.acoustid.as_deref(), Some("acoustid-abc"));
    assert_eq!(tags.replaygain_track_gain_db, Some(-3.5));
}
