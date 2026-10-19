//! Reference outputs that need no hardware.

mod null;
mod wav;

pub use null::NullOutput;
pub use wav::{read_wav, WavFileOutput, WavSink};
