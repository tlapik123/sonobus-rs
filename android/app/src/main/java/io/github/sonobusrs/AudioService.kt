package io.github.sonobusrs

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.net.ConnectivityManager
import android.net.Network
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.IntentCompat

/**
 * Foreground service for the whole app run: keeps the process (engine, network, hosted server)
 * alive while the activity is in the background, holds the MediaProjection and the capture,
 * the session locks, and shows the ongoing notification with a Stop action (quits SonoBus).
 */
class AudioService : Service() {
    companion object {
        private const val TAG = "sonobus"
        private const val CHANNEL = "session"
        private const val NOTIFICATION_ID = 1
        private const val ACTION_STOP = "io.github.sonobusrs.STOP"
        private const val EXTRA_CODE = "code"
        private const val EXTRA_DATA = "data"
        private const val EXTRA_UID = "uid"

        @Volatile var instance: AudioService? = null

        fun start(ctx: Context) {
            ctx.startForegroundService(Intent(ctx, AudioService::class.java))
        }

        /** After the consent dialog returned RESULT_OK. */
        fun startCapture(ctx: Context, resultCode: Int, data: Intent, uid: Int) {
            ctx.startForegroundService(
                Intent(ctx, AudioService::class.java)
                    .putExtra(EXTRA_CODE, resultCode)
                    .putExtra(EXTRA_DATA, data)
                    .putExtra(EXTRA_UID, uid),
            )
        }
    }

    val mainHandler = Handler(Looper.getMainLooper())
    private var projection: MediaProjection? = null
    private var capture: PlaybackCapture? = null
    private lateinit var locks: SessionLocks
    private val network = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(n: Network) = NativeBridge.onNetworkChanged(true)
        override fun onLost(n: Network) = NativeBridge.onNetworkChanged(false)
    }

    override fun onCreate() {
        super.onCreate()
        instance = this
        locks = SessionLocks(this)
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CHANNEL, getString(R.string.channel_name), NotificationManager.IMPORTANCE_LOW),
        )
        getSystemService(ConnectivityManager::class.java).registerDefaultNetworkCallback(network)
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            quit()
            return START_NOT_STICKY
        }
        val data = intent?.let { IntentCompat.getParcelableExtra(it, EXTRA_DATA, Intent::class.java) }
        // Android 14+: the mediaProjection foreground type must be in effect before getMediaProjection.
        foreground(projection = data != null)
        if (data != null) {
            beginCapture(intent.getIntExtra(EXTRA_CODE, 0), data, intent.getIntExtra(EXTRA_UID, -1))
        }
        return START_NOT_STICKY
    }

    private fun foreground(projection: Boolean) {
        var types = ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
            types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
        }
        if (projection) types = types or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
        val n = notification(projection)
        try {
            ServiceCompat.startForeground(this, NOTIFICATION_ID, n, types)
        } catch (e: RuntimeException) {
            // e.g. microphone type refused while in the background: keep playback only
            Log.w(TAG, "startForeground($types) failed: $e")
            ServiceCompat.startForeground(
                this, NOTIFICATION_ID, n,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK or
                    (if (projection) ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION else 0),
            )
        }
    }

    /** Re-evaluates the foreground types (after the microphone permission was granted). */
    fun refresh() = foreground(projection != null)

    private fun notification(capturing: Boolean): Notification {
        val flags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java), flags)
        val stop = PendingIntent.getService(this, 1, Intent(this, AudioService::class.java).setAction(ACTION_STOP), flags)
        return NotificationCompat.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(getString(if (capturing) R.string.notif_capturing else R.string.notif_running))
            .setContentIntent(open)
            .addAction(0, getString(R.string.stop), stop)
            .setOngoing(true)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .build()
    }

    private fun beginCapture(resultCode: Int, data: Intent, uid: Int) {
        stopCapture(null, report = false)
        try {
            val mpm = getSystemService(MediaProjectionManager::class.java)
            val p = mpm.getMediaProjection(resultCode, data)
                ?: throw IllegalStateException("no media projection granted")
            // registered before use (required from Android 14)
            p.registerCallback(object : MediaProjection.Callback() {
                override fun onStop() {
                    if (projection === p) {
                        stopCapture("Sharing was stopped by Android (revoked, screen locked, or another app started sharing)")
                    }
                }
            }, mainHandler)
            projection = p
            capture = PlaybackCapture(p, uid) { why -> mainHandler.post { stopCapture(why) } }.also { it.start() }
        } catch (e: Exception) {
            Log.w(TAG, "capture failed", e)
            stopCapture("Could not capture: ${e.message ?: e.javaClass.simpleName}")
        }
    }

    /** Stops capture; [reason] null = on request (reported as off), else reported as stopped. */
    fun stopCapture(reason: String?, report: Boolean = true) {
        val had = capture != null || projection != null
        capture?.stop()
        capture = null
        projection?.let { p ->
            projection = null
            p.stop()
        }
        if (report) {
            val code = if (reason == null) NativeBridge.CAPTURE_OFF else NativeBridge.CAPTURE_STOPPED
            NativeBridge.onCaptureState(code, 0, 0, 0, reason)
        }
        if (had) foreground(projection = false)
    }

    fun setSessionActive(active: Boolean) = locks.set(active)

    /** Swiping SonoBus away from recents quits it (see MainActivity). */
    override fun onTaskRemoved(rootIntent: Intent?) = quit()

    /** Stop action: save, stop everything, end the process (the UI cannot be recreated in it). */
    fun quit() {
        stopCapture(null)
        locks.set(false)
        NativeBridge.nativeSave()
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        stopSelf()
        NativeBridge.activity?.get()?.finishAndRemoveTask()
        android.os.Process.killProcess(android.os.Process.myPid())
    }

    override fun onDestroy() {
        getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(network)
        instance = null
        super.onDestroy()
    }
}
