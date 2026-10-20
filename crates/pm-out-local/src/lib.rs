//! Local outputs: the USB DAC or HDMI port on the node itself.
//!
//! Handoff §4 names three bit-perfect backends: ALSA `hw:` on Linux, WASAPI
//! exclusive on Windows and CoreAudio hog/integer mode on macOS. ALSA is
//! written (feature `alsa`); for Windows and macOS a shared-mode backend
//! through `cpal` exists (feature `cpal`) so the controller plays locally
//! there, with its ceiling shown honestly as lossless-up-to, not bit-perfect;
//! the exclusive/hog-mode backends are still to come.
//!
//! **Verification status:** the `alsa` feature is built and tested on a FiiO
//! KA17 (M1 hardware acceptance): bit-perfect PCM to the DAC, byte-identical
//! through `snd-aloop` at 16/44.1, 24/96 and 24/192. Native DSD is sent
//! correctly but plays silent on that DAC (`docs/device-db/fiio-ka17.yaml`).
//! The non-feature build is what CI exercises.

#[cfg(feature = "alsa")]
pub mod alsa;

/// Shared-mode output through the operating system (CoreAudio, WASAPI, ALSA
/// via `cpal`): lossless up to the device rate, not bit-perfect, for the
/// native controller on desktops. Behind the `cpal` feature.
#[cfg(feature = "cpal")]
pub mod cpal_out;

/// Device names this crate will open. `plughw:` and `default` go through
/// ALSA's conversion plugins, which silently resample and dither; refusing
/// them keeps constraint 5 (bit-perfect) enforceable.
pub fn is_direct_hw_name(name: &str) -> bool {
    name.starts_with("hw:")
}

/// Silence alsa-lib's own stderr chatter ("Found no matching channel map"
/// for every plugin it enumerates). The node reports device errors itself;
/// call once at start of any process that opens ALSA devices.
#[cfg(feature = "alsa")]
pub fn quiet_alsa_lib() {
    // alsa-lib's handler is variadic, which stable Rust cannot define. A
    // non-variadic function that ignores every argument is ABI-compatible
    // for the fixed arguments on every platform alsa-lib runs on (the extra
    // arguments are simply never read), so the cast below is sound in
    // practice and is what C programs do when they pass `(void (*)())` here.
    unsafe extern "C" fn silent(
        _file: *const std::os::raw::c_char,
        _line: std::os::raw::c_int,
        _function: *const std::os::raw::c_char,
        _err: std::os::raw::c_int,
        _fmt: *const std::os::raw::c_char,
    ) {
    }
    type Fixed = unsafe extern "C" fn(
        *const std::os::raw::c_char,
        std::os::raw::c_int,
        *const std::os::raw::c_char,
        std::os::raw::c_int,
        *const std::os::raw::c_char,
    );
    // SAFETY: see above; the handler never touches its arguments.
    unsafe {
        let handler: alsa_sys::snd_lib_error_handler_t = Some(std::mem::transmute::<
            Fixed,
            unsafe extern "C" fn(
                *const std::os::raw::c_char,
                std::os::raw::c_int,
                *const std::os::raw::c_char,
                std::os::raw::c_int,
                *const std::os::raw::c_char,
                ...
            ),
        >(silent as Fixed));
        alsa_sys::snd_lib_error_set_handler(handler);
    }
}

#[cfg(not(feature = "alsa"))]
pub fn quiet_alsa_lib() {}

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
