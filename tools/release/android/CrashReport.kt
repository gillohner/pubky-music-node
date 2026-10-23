// Copied into the generated Android project by tools/release/android-patch.py, next to
// MainActivity.kt (the package line is rewritten to match).
//
// A crash report the owner can read without adb (STEWARD-FIX-android-crash.md): an uncaught
// Java exception, a Rust panic (src/lib.rs writes the same file), and on Android 11+ the
// system's own record of why the app's last process ended (a native crash or an ANR included,
// with its trace). All of it goes to files/crash-report.txt and to logcat under "PubkyMusic";
// the connect page shows it on the next launch with Copy and Share. Nothing is sent anywhere.
package to.synonym.pubkymusic

import android.app.Activity
import android.app.ActivityManager
import android.app.Application
import android.app.ApplicationExitInfo
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

class PubkyMusicApp : Application() {
    override fun onCreate() {
        super.onCreate()
        CrashReport.install(this)
    }
}

object CrashReport {
    const val FILE = "crash-report.txt"
    private const val TAG = "PubkyMusic"
    private const val MAX_TRACE = 64 * 1024

    @JvmStatic
    fun install(context: Context) {
        val ctx = context.applicationContext ?: context
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { thread, e ->
            try {
                write(ctx, "Java exception on thread \"${thread.name}\"", Log.getStackTraceString(e))
            } catch (_: Throwable) {
            }
            previous?.uncaughtException(thread, e)
        }
        try {
            lastExit(ctx)
        } catch (e: Throwable) {
            Log.w(TAG, "could not read the last exit reason: $e")
        }
    }

    private fun stamp(ms: Long): String =
        SimpleDateFormat("yyyy-MM-dd HH:mm:ss Z", Locale.ROOT).format(Date(ms))

    private fun header(ctx: Context): String {
        val version = try {
            ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName
        } catch (_: Throwable) {
            "?"
        }
        return "Pubky Music $version on ${Build.MANUFACTURER} ${Build.MODEL}, Android ${Build.VERSION.RELEASE} " +
            "(API ${Build.VERSION.SDK_INT}), ${Build.SUPPORTED_ABIS.joinToString()}"
    }

    @JvmStatic
    fun write(ctx: Context, title: String, body: String) {
        val text = "== ${stamp(System.currentTimeMillis())}: $title\n${header(ctx)}\n$body\n\n"
        Log.e(TAG, "$title\n$body")
        File(ctx.filesDir, FILE).appendText(text)
    }

    /** Android 11+: why the previous process ended, when it was a crash or a freeze. */
    private fun lastExit(ctx: Context) {
        if (Build.VERSION.SDK_INT < 30) return
        val am = ctx.getSystemService(ActivityManager::class.java) ?: return
        // The app's own process: the WebView's sandboxed renderers are processes of this app too,
        // and die with it
        val info = am.getHistoricalProcessExitReasons(ctx.packageName, 0, 8)
            .firstOrNull { it.processName == ctx.packageName } ?: return
        val prefs = ctx.getSharedPreferences("pubky-music-crash", Context.MODE_PRIVATE)
        if (info.timestamp <= prefs.getLong("seen", 0)) return
        prefs.edit().putLong("seen", info.timestamp).apply()
        val what = when (info.reason) {
            ApplicationExitInfo.REASON_CRASH -> "crashed (Java)"
            ApplicationExitInfo.REASON_CRASH_NATIVE -> "crashed (native code)"
            ApplicationExitInfo.REASON_ANR -> "stopped responding"
            ApplicationExitInfo.REASON_INITIALIZATION_FAILURE -> "failed to start"
            else -> return
        }
        val trace = try {
            info.traceInputStream?.use { s -> readable(s.readBytes()) }
        } catch (_: Throwable) {
            null
        }
        write(
            ctx,
            "The app $what at ${stamp(info.timestamp)}: ${info.description ?: "no description"}",
            "status ${info.status}, importance ${info.importance}" + (trace?.let { "\n$it" } ?: "")
        )
    }

    /**
     * An ANR trace is text; a native crash's (Android 12+) is a binary tombstone. Of that, keep
     * what a person can read: its runs of printable characters (libraries, functions, the abort
     * message), as `strings` would.
     */
    private fun readable(b: ByteArray): String {
        val printable = b.count { it in 0x20..0x7e || it == 0x0a.toByte() || it == 0x09.toByte() }
        if (b.isNotEmpty() && printable * 10 >= b.size * 9) return String(b, 0, minOf(b.size, MAX_TRACE))
        val out = StringBuilder()
        val run = StringBuilder()
        for (x in b) {
            if (x in 0x20..0x7e) {
                run.append(x.toInt().toChar())
            } else {
                if (run.length >= 6) out.append(run).append('\n')
                run.setLength(0)
            }
            if (out.length > MAX_TRACE) break
        }
        if (run.length >= 6) out.append(run)
        return out.toString()
    }

    /** The share sheet, for the connect page's "Share" button (called over JNI). */
    @JvmStatic
    fun share(activity: Activity, text: String) {
        val send = Intent(Intent.ACTION_SEND)
            .setType("text/plain")
            .putExtra(Intent.EXTRA_SUBJECT, "Pubky Music crash report")
            .putExtra(Intent.EXTRA_TEXT, text)
        activity.startActivity(Intent.createChooser(send, "Share the crash report"))
    }
}
