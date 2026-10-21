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

/// Android drops multicast packets unless an app holds a Wi-Fi multicast lock (it costs battery, so
/// only while looking for nodes). Needs `CHANGE_WIFI_MULTICAST_STATE` (tools/release/android-patch.py).
/// SSDP answers are unicast and arrive without it; mDNS answers need it.
pub struct MulticastLock {
    window: Option<tauri::WebviewWindow>,
}

/// The lock while held (one at a time).
static LOCK: std::sync::Mutex<Option<jni021::objects::GlobalRef>> = std::sync::Mutex::new(None);

impl MulticastLock {
    pub async fn acquire(app: &tauri::AppHandle) -> Self {
        use tauri::Manager;
        let Some(window) = app.get_webview_window(crate::MAIN) else {
            return Self { window: None };
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let scheduled = window.with_webview(move |wv| {
            wv.jni_handle().exec(move |env, activity, _webview| {
                match take_lock(env, activity) {
                    Ok(lock) => {
                        if let Ok(mut held) = LOCK.lock() {
                            *held = Some(lock);
                        }
                    }
                    Err(e) => tracing::warn!("no Wi-Fi multicast lock ({e}); mDNS may find nothing"),
                }
                let _ = tx.send(());
            });
        });
        if scheduled.is_ok() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx).await;
        }
        Self { window: Some(window) }
    }
}

impl Drop for MulticastLock {
    fn drop(&mut self) {
        let Some(window) = self.window.take() else { return };
        let _ = window.with_webview(|wv| {
            wv.jni_handle().exec(|env, _activity, _webview| {
                let held = LOCK.lock().ok().and_then(|mut l| l.take());
                if let Some(lock) = held {
                    let _ = env.call_method(lock.as_obj(), "release", "()V", &[]);
                }
            });
        });
    }
}

fn take_lock(
    env: &mut jni021::JNIEnv,
    activity: &jni021::objects::JObject,
) -> jni021::errors::Result<jni021::objects::GlobalRef> {
    use jni021::objects::JValue;
    let ctx = env
        .call_method(
            activity,
            "getApplicationContext",
            "()Landroid/content/Context;",
            &[],
        )?
        .l()?;
    let service = env.new_string("wifi")?;
    let wifi = env
        .call_method(
            &ctx,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&service)],
        )?
        .l()?;
    if wifi.is_null() {
        return Err(jni021::errors::Error::NullPtr("no Wi-Fi service"));
    }
    let tag = env.new_string("pubky-music-discovery")?;
    let lock = env
        .call_method(
            &wifi,
            "createMulticastLock",
            "(Ljava/lang/String;)Landroid/net/wifi/WifiManager$MulticastLock;",
            &[JValue::Object(&tag)],
        )?
        .l()?;
    env.call_method(&lock, "setReferenceCounted", "(Z)V", &[JValue::Bool(0)])?;
    env.call_method(&lock, "acquire", "()V", &[])?;
    env.new_global_ref(lock)
}
