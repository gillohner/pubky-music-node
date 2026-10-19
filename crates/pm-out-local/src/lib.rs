//! Local outputs: the USB DAC or HDMI port on the node itself.
//!
//! Handoff §4 names three bit-perfect backends: ALSA `hw:` on Linux, WASAPI
//! exclusive on Windows and CoreAudio hog/integer mode on macOS. Only ALSA is
//! written so far, behind the `alsa` cargo feature.
//!
//! **Verification status:** this machine has no `libasound` headers, so the
//! `alsa` feature has not been compiled or run here. Treat [`alsa`] as a
//! careful draft to be built and tested on the node at M1 (byte-identical
//! loopback test). The non-feature build is what CI exercises.

#[cfg(feature = "alsa")]
pub mod alsa;

/// Device names this crate will open. `plughw:` and `default` go through
/// ALSA's conversion plugins, which silently resample and dither; refusing
/// them keeps constraint 5 (bit-perfect) enforceable.
pub fn is_direct_hw_name(name: &str) -> bool {
    name.starts_with("hw:")
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_hw_names() {
        assert!(super::is_direct_hw_name("hw:0,0"));
        assert!(super::is_direct_hw_name("hw:CARD=DAC,DEV=0"));
        assert!(!super::is_direct_hw_name("plughw:0,0"));
        assert!(!super::is_direct_hw_name("default"));
    }
}
