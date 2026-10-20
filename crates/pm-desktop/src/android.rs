//! Android-only setup.
//!
//! The pkarr relay client behind the Pubky Ring sign-in verifies TLS with
//! `rustls-platform-verifier`, which on Android needs the JVM and an app
//! context before the first handshake; without them the first handshake
//! panics ("Expect rustls-platform-verifier to be initialized"). Its Kotlin
//! classes come from the verifier's Maven archive, added to the generated
//! Gradle project by `tools/release/android-patch.py`.
//!
//! Tauri hands over `jni` 0.21 objects and the verifier uses `jni` 0.22, so
//! the two meet through raw pointers.

use std::ffi::c_void;

/// Initialise the platform verifier from the webview's activity. Runs on
/// the activity's thread, long before anyone can press "Sign in".
pub fn init_tls(window: &tauri::WebviewWindow) {
    let scheduled = window.with_webview(|wv| {
        wv.jni_handle().exec(|env, activity, _webview| {
            let raw_env = env.get_raw() as *mut c_void;
            let raw_ctx = activity.as_raw() as *mut c_void;
            // SAFETY: both pointers belong to the live JNI frame of this
            // callback, on this thread.
            if unsafe { init_verifier(raw_env, raw_ctx) } {
                tracing::info!("TLS platform verifier ready");
            } else {
                tracing::error!("TLS platform verifier failed to initialise; Pubky Ring sign-in will fail");
            }
        });
    });
    if let Err(e) = scheduled {
        tracing::error!("cannot reach the webview to set up TLS: {e}");
    }
}

/// # Safety
/// `raw_env` must be the current thread's `JNIEnv` and `raw_ctx` a live
/// local or global reference to an Android `Context`.
unsafe fn init_verifier(raw_env: *mut c_void, raw_ctx: *mut c_void) -> bool {
    // SAFETY: the caller guarantees a valid JNIEnv for this thread.
    let mut env = unsafe { jni::EnvUnowned::from_raw(raw_env as *mut jni::sys::JNIEnv) };
    let outcome = env.with_env(|env| -> Result<(), jni::errors::Error> {
        // SAFETY: the caller guarantees a live reference to a Context.
        let ctx = unsafe { jni::objects::JObject::from_raw(env, raw_ctx as jni::sys::jobject) };
        rustls_platform_verifier::android::init_with_env(env, ctx)
    });
    matches!(outcome.into_outcome(), jni::Outcome::Ok(()))
}
