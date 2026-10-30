//! Reference outputs that need no hardware, and the group output that
//! plays one stream to several outputs.

mod group;
mod null;
mod pull;
mod wav;

pub use group::{intersect_caps, GroupOutput, GroupSink};
pub use null::NullOutput;
pub use pull::{
    pack_pcm, wav_header, wav_header_continuous, LiveStream, PullOutput, LISTENER_TIMEOUT, PROBE_KEEP_BYTES,
    PROBE_TIME, REATTACH_GRACE,
};
pub use wav::{read_wav, WavFileOutput, WavSink};
