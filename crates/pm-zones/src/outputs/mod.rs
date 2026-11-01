//! Reference outputs that need no hardware, and the group output that
//! plays one stream to several outputs.

mod group;
mod null;
mod pull;
mod wav;

pub use group::{intersect_caps, GroupOutput, GroupSink};
pub use null::NullOutput;
pub use pull::{
    fits_in, pack_pcm, sized_stream_frames, wav_header, wav_header_continuous, wav_header_sized_stream,
    LiveStream, Missed, PullOutput, LISTENER_TIMEOUT, PROBE_KEEP_BYTES, PROBE_TIME, REATTACH_GRACE,
    SIZED_STREAM_BYTES,
};
pub use wav::{read_wav, WavFileOutput, WavSink};
