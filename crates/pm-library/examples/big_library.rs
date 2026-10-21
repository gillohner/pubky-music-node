//! A synthetic library for speed checks (steward next-3, item 1): generated names, a hundredth of
//! a second of silence per file, nothing copyrighted.
//!
//! `cargo run -p pm-library --example big_library -- <dir> [tracks]` (default 50,000 tracks:
//! 5,000 albums of ten, 1,500 artists). About one album in five is untagged (a title from the file
//! name only); about one tagged track in three carries a (made-up) MusicBrainz recording id.
//! The same arguments always give the same files, so timings can be compared between runs.

use id3::TagLike;
use std::path::{Path, PathBuf};

const WORDS: &[&str] = &[
    "amber",
    "night",
    "river",
    "glass",
    "echo",
    "velvet",
    "north",
    "signal",
    "paper",
    "ocean",
    "silver",
    "winter",
    "garden",
    "motion",
    "orbit",
    "static",
    "hollow",
    "golden",
    "shadow",
    "city",
    "dream",
    "fire",
    "lights",
    "machine",
    "summer",
    "storm",
    "heart",
    "electric",
    "stone",
    "blue",
    "wild",
    "quiet",
    "neon",
    "desert",
    "morning",
    "falling",
    "secret",
    "cold",
    "radio",
    "satellite",
    "honey",
    "midnight",
    "crystal",
    "forest",
    "lonely",
    "highway",
    "thunder",
    "violet",
    "harbor",
    "distant",
    "sugar",
    "broken",
    "mirror",
    "island",
    "copper",
    "feather",
    "Café",
    "Sœur",
    "Mädchen",
    "Ñandú",
    "Øresund",
];
const GENRES: &[&str] = &[
    "Rock",
    "Jazz",
    "Electronic",
    "Classical",
    "Hip Hop",
    "Folk",
    "Ambient",
    "Soul",
    "Pop",
    "Metal",
];

/// A small deterministic generator (no extra dependencies).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
    fn words(&mut self, n: u64) -> String {
        let n = 1 + self.next() % n;
        let mut out: Vec<String> = Vec::new();
        for _ in 0..n {
            out.push(self.pick(WORDS).to_string());
        }
        let s = out.join(" ");
        let mut c = s.chars();
        c.next()
            .map(|f| f.to_uppercase().chain(c).collect())
            .unwrap_or_default()
    }
    fn uuid(&mut self) -> String {
        let a = self.next();
        let b = self.next();
        format!(
            "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
            a >> 32,
            (a >> 16) & 0xffff,
            a & 0xfff,
            b >> 52,
            b & 0xffff_ffff_ffff
        )
    }
}

fn safe(s: &str) -> String {
    s.replace(['/', '\\'], "-")
}

fn silent_wav(path: &Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("create wav");
    for _ in 0..441 * 2 {
        w.write_sample(0i16).expect("write");
    }
    w.finalize().expect("finalize");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("usage: big_library <dir> [tracks]"));
    let tracks: u64 = args.next().map(|n| n.parse().expect("tracks")).unwrap_or(50_000);
    let albums = (tracks / 10).max(1);
    let artists = (albums * 3 / 10).max(1);
    let mut rng = Rng(0x5eed_cafe_f00d_d00d);
    let artist_names: Vec<String> = (0..artists)
        .map(|i| format!("{} {}", rng.words(2), i + 1))
        .collect();
    let mut made = 0u64;
    for al in 0..albums {
        let artist = &artist_names[(al % artists) as usize];
        let album = format!("{} ({})", rng.words(3), al + 1);
        let year = 1960 + (rng.next() % 64) as i32;
        let genre = rng.pick(GENRES);
        let tagged = !rng.next().is_multiple_of(5);
        let folder = dir.join(safe(artist)).join(safe(&album));
        std::fs::create_dir_all(&folder).expect("mkdir");
        for n in 1..=10u32 {
            if made >= tracks {
                break;
            }
            made += 1;
            let title = rng.words(4);
            let path = folder.join(format!("{n:02} - {}.wav", safe(&title)));
            if path.exists() {
                continue;
            }
            silent_wav(&path);
            if !tagged {
                continue;
            }
            let mut t = id3::Tag::new();
            t.set_title(&title);
            t.set_artist(artist);
            t.set_album(&album);
            t.set_album_artist(artist);
            t.set_track(n);
            t.set_total_tracks(10);
            t.set_year(year);
            t.set_genre(genre);
            if rng.next().is_multiple_of(3) {
                t.add_frame(id3::frame::ExtendedText {
                    description: "MusicBrainz Track Id".into(),
                    value: rng.uuid(),
                });
            }
            t.write_to_path(&path, id3::Version::Id3v24).expect("tag");
        }
    }
    println!(
        "{made} tracks, {albums} albums, {artists} artists in {}",
        dir.display()
    );
}
