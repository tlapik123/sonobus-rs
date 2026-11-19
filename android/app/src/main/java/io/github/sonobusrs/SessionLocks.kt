package io.github.sonobusrs

import android.content.Context
import android.net.wifi.WifiManager
import android.os.PowerManager

/** Held while a session is active: multicast receive, low-latency Wi-Fi, CPU awake with the screen off. */
class SessionLocks(ctx: Context) {
    private val wifi = ctx.applicationContext.getSystemService(WifiManager::class.java)
    private val multicast = wifi.createMulticastLock("sonobus:multicast").apply { setReferenceCounted(false) }
    private val lowLatency = wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "sonobus:low-latency")
        .apply { setReferenceCounted(false) }
    private val wake = ctx.getSystemService(PowerManager::class.java)
        .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "sonobus:session").apply { setReferenceCounted(false) }

    fun set(active: Boolean) {
        if (active) {
            multicast.acquire()
            lowLatency.acquire()
            wake.acquire()
        } else {
            if (multicast.isHeld) multicast.release()
            if (lowLatency.isHeld) lowLatency.release()
            if (wake.isHeld) wake.release()
        }
    }
}
