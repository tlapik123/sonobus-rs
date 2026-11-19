package io.github.sonobusrs

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import androidx.activity.addCallback
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import com.google.androidgamesdk.GameActivity
import java.lang.ref.WeakReference

/**
 * Hosts the egui surface (android-activity drives it from `android_main` in Rust). Not
 * recreated during the process lifetime: config changes are handled in place, Back moves the
 * task to the background, and finishing the activity quits the app.
 */
class MainActivity : GameActivity() {
    private var pendingUid = -1
    private val consent = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { r ->
        val data = r.data
        if (r.resultCode == RESULT_OK && data != null) {
            AudioService.startCapture(this, r.resultCode, data, pendingUid)
        } else {
            NativeBridge.onCaptureState(NativeBridge.CAPTURE_STOPPED, 0, 0, 0, "Sharing was not allowed")
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        NativeBridge.appContext = applicationContext
        NativeBridge.activity = WeakReference(this)
        super.onCreate(savedInstanceState)
        // system bars, display cutout and soft keyboard: the egui layout keeps out of them
        ViewCompat.setOnApplyWindowInsetsListener(window.decorView) { v, insets ->
            val i = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout() or
                    WindowInsetsCompat.Type.ime(),
            )
            NativeBridge.onInsets(i.left, i.top, i.right, i.bottom)
            ViewCompat.onApplyWindowInsets(v, insets)
        }
        onBackPressedDispatcher.addCallback(this) { moveTaskToBack(true) }
        openLink(intent)
        AudioService.start(this)
        val wanted = buildList {
            add(Manifest.permission.RECORD_AUDIO)
            if (Build.VERSION.SDK_INT >= 33) add(Manifest.permission.POST_NOTIFICATIONS)
        }.filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        if (wanted.isNotEmpty()) requestPermissions(wanted.toTypedArray(), 1)
    }

    /** singleTask: a link opened while the app runs arrives here. */
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        openLink(intent)
    }

    /** An invite link (manifest intent filter): Rust shows it and asks before joining. */
    private fun openLink(intent: Intent?) {
        val url = intent?.takeIf { it.action == Intent.ACTION_VIEW }?.dataString ?: return
        NativeBridge.onOpenUrl(url)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, results)
        val i = permissions.indexOf(Manifest.permission.RECORD_AUDIO)
        if (i >= 0) {
            val granted = results.getOrNull(i) == PackageManager.PERMISSION_GRANTED
            NativeBridge.onPermission(granted)
            if (granted) AudioService.instance?.refresh()
        }
    }

    /** Shows the system consent dialog; the service starts capturing when it is accepted. */
    fun requestCapture(uid: Int) {
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            NativeBridge.onCaptureState(NativeBridge.CAPTURE_STOPPED, 0, 0, 0, "Allow the microphone permission first (Android requires it for sound capture)")
            requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), 1)
            return
        }
        pendingUid = uid
        val mpm = getSystemService(MediaProjectionManager::class.java)
        // Android 14+: offer only "entire screen" (single-app sharing would not cover system sound)
        val intent: Intent = if (Build.VERSION.SDK_INT >= 34) {
            mpm.createScreenCaptureIntent(MediaProjectionConfig.createConfigForDefaultDisplay())
        } else {
            mpm.createScreenCaptureIntent()
        }
        consent.launch(intent)
    }

    override fun onStop() {
        NativeBridge.nativeSave()
        super.onStop()
    }

    override fun onDestroy() {
        // winit cannot start a second UI in this process: a finished activity ends the app
        if (isFinishing) AudioService.instance?.quit()
        super.onDestroy()
    }
}
