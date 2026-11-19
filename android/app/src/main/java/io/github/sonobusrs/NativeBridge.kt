package io.github.sonobusrs

import android.content.Context
import java.lang.ref.WeakReference
import java.nio.ByteBuffer
import kotlin.concurrent.thread

/**
 * JNI both ways with crates/sonobus-android (src/android.rs). `external` functions are
 * implemented in Rust; the plain `@JvmStatic` functions are called from Rust, on any thread.
 */
object NativeBridge {
    const val CAPTURE_OFF = 0
    const val CAPTURE_STARTED = 2
    const val CAPTURE_STOPPED = 3

    init {
        System.loadLibrary("sonobus_android")
        nativeInit()
    }

    @Volatile lateinit var appContext: Context
    @Volatile var activity: WeakReference<MainActivity>? = null

    @JvmStatic external fun nativeInit()
    @JvmStatic external fun onCaptureData(buf: ByteBuffer, bytes: Int)
    @JvmStatic external fun onCaptureState(code: Int, sampleRate: Int, channels: Int, frames: Int, message: String?)
    @JvmStatic external fun onCaptureLatency(ms: Float)
    @JvmStatic external fun onApps(list: String)
    @JvmStatic external fun onInsets(left: Int, top: Int, right: Int, bottom: Int)
    @JvmStatic external fun onPermission(granted: Boolean)
    @JvmStatic external fun onNetworkChanged(available: Boolean)
    @JvmStatic external fun nativeSave()
    /** A `sonobus2://join/...` link the app was opened with (camera QR scan, tapped link). */
    @JvmStatic external fun onOpenUrl(url: String)

    /** Asks for consent (needs the visible activity), then the service captures [uid] (-1 = system sound). */
    @JvmStatic
    fun startCapture(uid: Int) {
        val a = activity?.get()
        if (a == null) {
            onCaptureState(CAPTURE_STOPPED, 0, 0, 0, "Open SonoBus to allow sharing")
            return
        }
        a.runOnUiThread { a.requestCapture(uid) }
    }

    @JvmStatic
    fun stopCapture() {
        AudioService.instance?.let { s -> s.mainHandler.post { s.stopCapture(null) } }
    }

    @JvmStatic
    fun loadApps() {
        val ctx = appContext
        thread(name = "sonobus-apps") { onApps(AppList.encode(AppList.launchable(ctx), ctx.packageName)) }
    }

    @JvmStatic
    fun setSessionActive(active: Boolean) {
        AudioService.instance?.let { s -> s.mainHandler.post { s.setSessionActive(active) } }
    }
}
