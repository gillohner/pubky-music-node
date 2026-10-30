//! Zones and outputs (handoff §6.4): the first thing to build.
//!
//! * [`Output`] is one place audio can go: a DAC, a network endpoint, a file.
//!   Every protocol in §7 implements it.
//! * [`AudioSink`] is an open output accepting [`pm_model::AudioBlock`]s in one
//!   negotiated [`pm_model::AudioFormat`]. Its methods block, so a zone drives it from a
//!   dedicated thread; the DAC's clock is the master and the sink pulls
//!   (handoff §6.1 principle 2).
//! * [`registry::OutputRegistry`] is where the node collects discovered outputs.
//! * [`zone::Zone`] is an output plus a [`queue::Queue`] and a transport,
//!   driven by its own audio thread: gapless joins, pause/seek, position,
//!   state and events. It pulls audio through the [`source`] traits, which
//!   `pm-audio` implements, so this crate stays codec-free.
//!
//! Two reference outputs ship here: [`outputs::NullOutput`] (counts frames,
//! drops them) and [`outputs::WavFileOutput`] (writes a bit-exact WAV file,
//! the basis of the byte-identical test in M1).

mod engine;
pub mod outputs;
pub mod queue;
pub mod registry;
pub mod sink;
pub mod source;
pub mod zone;

pub use queue::{FetchLimit, Insert, Queue, QueueItem, Repeat};
pub use registry::OutputRegistry;
pub use sink::{
    AudioSink, BufferLevel, ClockSource, Describe, MemberStatus, Output, OutputId, OutputInfo, SinkError,
    SyncClass, TrackMeta,
};
pub use source::{
    FrameSource, Processor, ReplayGain, SourceError, SourceFactory, StreamInfo, WithReplayGain,
};
pub use zone::{NowPlaying, Transport, Zone, ZoneEvent, ZoneId, ZoneState, Zones};
