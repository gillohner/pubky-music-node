//! Set Vorbis comments on a FLAC file (test data for a dev library):
//!
//! ```sh
//! cargo run -p pm-library --example tag-flac -- file.flac ARTIST=Queen ALBUM="A Night at the Opera" TITLE=… TRACKNUMBER=11
//! ```

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: tag-flac FILE KEY=VALUE…");
    let mut tag = metaflac::Tag::read_from_path(&path).expect("read FLAC");
    for kv in args {
        let (k, v) = kv.split_once('=').expect("KEY=VALUE");
        tag.set_vorbis(k.to_ascii_uppercase(), vec![v.to_string()]);
    }
    tag.save().expect("write FLAC");
}
