//! Zones and outputs (handoff §6.4): the first thing to build.
//!
//! * [`Output`] is one place audio can go: a DAC, a network endpoint, a file.
//!   Every protocol in §7 implements it.
//! * [`AudioSink`] is an open output accepting [`AudioBlock`]s in one
//!   negotiated [`AudioFormat`]. Its methods block, so a zone drives it from a
//!   dedicated thread; the DAC's clock is the master and the sink pulls
//!   (handoff §6.1 principle 2).
//! * [`registry::OutputRegistry`] is where the node collects discovered outputs.
//!
//! Two reference outputs ship here: [`outputs::NullOutput`] (counts frames,
//! drops them) and [`outputs::WavFileOutput`] (writes a bit-exact WAV file,
//! the basis of the byte-identical test in M1).

pub mod outputs;
pub mod registry;
pub mod sink;

pub use registry::OutputRegistry;
pub use sink::{AudioSink, BufferLevel, ClockSource, Output, OutputId, OutputInfo, SinkError};
