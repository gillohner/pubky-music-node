//! Reference outputs that need no hardware, and the group output that
//! plays one stream to several outputs.

mod group;
mod null;
mod wav;

pub use group::{intersect_caps, GroupOutput, GroupSink};
pub use null::NullOutput;
pub use wav::{read_wav, WavFileOutput, WavSink};
