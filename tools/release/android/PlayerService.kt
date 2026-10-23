// Copied into the generated Android project by tools/release/android-patch.py,
// next to MainActivity.kt (the package line is rewritten to match).
//
// Keeps the app's built-in player alive while the app is in the background:
// without a foreground service Android freezes the process within seconds of
// leaving the app, and the node's stream to this phone stops. Started by the
// Rust side (src/android.rs) once the player is registered with a node; it
// must start while the app is on screen (Android 12+ refuses otherwise).
package to.synonym.pubkymusic

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager

class PlayerService : Service() {
    private var wake: PowerManager.WakeLock? = null
    private var wifi: WifiManager.WifiLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, "Player", NotificationManager.IMPORTANCE_LOW).apply {
                description = "Shown while this phone is a zone of your node"
            }
        )
        val launch = packageManager.getLaunchIntentForPackage(packageName)
        val open = PendingIntent.getActivity(this, 0, launch, PendingIntent.FLAG_IMMUTABLE)
        val text = intent?.getStringExtra(EXTRA_TEXT) ?: "This phone is a zone"
        val n = Notification.Builder(this, CHANNEL)
            // A monochrome status-bar icon: the launcher icon is an adaptive icon, which some
            // phones refuse as a small icon ("Bad notification for startForeground", a crash).
            .setSmallIcon(R.drawable.ic_stat_pubky_music)
            .setContentTitle("Pubky Music")
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
            .build()
        try {
            if (Build.VERSION.SDK_INT >= 29) {
                startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK)
            } else {
                startForeground(1, n)
            }
        } catch (e: Exception) {
            // Refused (a missing permission, a background start, a system that wants more):
            // no background playback, but the app carries on.
            CrashReport.write(this, "The player service could not start", android.util.Log.getStackTraceString(e))
            stopSelf()
            return START_NOT_STICKY
        }
        hold()
        return START_NOT_STICKY
    }

    /// The stream arrives over Wi-Fi with the screen off: keep the CPU and the radio awake.
    private fun hold() {
        if (wake == null) {
            val pm = getSystemService(PowerManager::class.java)
            wake = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "pubky-music:player").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
        if (wifi == null) {
            val wm = applicationContext.getSystemService(WifiManager::class.java)
            @Suppress("DEPRECATION")
            val mode = if (Build.VERSION.SDK_INT >= 29) WifiManager.WIFI_MODE_FULL_LOW_LATENCY
                       else WifiManager.WIFI_MODE_FULL_HIGH_PERF
            wifi = wm.createWifiLock(mode, "pubky-music:player").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
    }

    /// Swiping the app away ends the zone too.
    override fun onTaskRemoved(rootIntent: Intent?) {
        stopSelf()
    }

    override fun onDestroy() {
        wake?.release()
        wifi?.release()
        wake = null
        wifi = null
        super.onDestroy()
    }

    companion object {
        const val CHANNEL = "player"
        const val EXTRA_TEXT = "text"
        private const val NOTIFY = "android.permission.POST_NOTIFICATIONS"

        @JvmStatic
        fun start(activity: Activity, text: String) {
            // Android 13+: without this the service still runs, but its notification is hidden.
            if (Build.VERSION.SDK_INT >= 33 &&
                activity.checkSelfPermission(NOTIFY) != PackageManager.PERMISSION_GRANTED
            ) {
                activity.requestPermissions(arrayOf(NOTIFY), 7)
            }
            val i = Intent(activity, PlayerService::class.java).putExtra(EXTRA_TEXT, text)
            try {
                activity.startForegroundService(i)
            } catch (e: Exception) {
                CrashReport.write(activity, "The player service was refused", android.util.Log.getStackTraceString(e))
            }
        }

        @JvmStatic
        fun stop(context: Context) {
            context.stopService(Intent(context, PlayerService::class.java))
        }
    }
}
