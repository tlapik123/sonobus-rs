package io.github.sonobusrs

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.AudioTimestamp
import android.media.projection.MediaProjection
import android.os.Process
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Records what other apps play ([uid] < 0: every app; else that app only) and hands it to Rust
 * from a dedicated urgent-audio thread: float PCM read into one direct buffer, passed by
 * reference (no per-buffer allocation, no JNI array copy).
 */
class PlaybackCapture(
    private val projection: MediaProjection,
    private val uid: Int,
    private val onFailed: (String) -> Unit,
) {
    companion object {
        const val RATE = 48000
        /** Frames per read: 5 ms. */
        const val FRAMES = 240
        /** Reads between latency reports (about 0.5 s). */
        const val LATENCY_EVERY = 100
    }

    @Volatile private var running = false
    private var record: AudioRecord? = null
    private var thread: Thread? = null

    /** Starts recording and reports `CAPTURE_STARTED` with the actual format. Throws on failure. */
    @SuppressLint("MissingPermission") // checked by MainActivity before consent
    fun start() {
        val format = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_FLOAT)
            .setSampleRate(RATE)
            .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
            .build()
        // Usages other apps can let us capture. Our own playback is excluded twice: the manifest
        // forbids capturing it (allowAudioPlaybackCapture=false) and system capture leaves out
        // our uid. (Matching and excluding uids cannot be combined; one app is matched alone.)
        val config = AudioPlaybackCaptureConfiguration.Builder(projection)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
        if (uid >= 0) config.addMatchingUid(uid) else config.excludeUid(Process.myUid())
        val min = AudioRecord.getMinBufferSize(RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_FLOAT)
        val rec = AudioRecord.Builder()
            .setAudioFormat(format)
            .setBufferSizeInBytes(maxOf(min, 4 * FRAMES * Pcm.frameBytes(2)))
            .setAudioPlaybackCaptureConfig(config.build())
            .build()
        if (rec.state != AudioRecord.STATE_INITIALIZED) {
            rec.release()
            throw IllegalStateException("the recorder did not initialise")
        }
        record = rec
        val channels = rec.channelCount
        val rate = rec.sampleRate
        rec.startRecording()
        running = true
        thread = Thread({ loop(rec, channels, rate) }, "sonobus-capture").apply { start() }
        NativeBridge.onCaptureState(NativeBridge.CAPTURE_STARTED, rate, channels, FRAMES, null)
    }

    private fun loop(rec: AudioRecord, channels: Int, rate: Int) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
        val frameBytes = Pcm.frameBytes(channels)
        val buf = ByteBuffer.allocateDirect(FRAMES * frameBytes).order(ByteOrder.nativeOrder())
        val ts = AudioTimestamp()
        var framesRead = 0L
        var reads = 0
        while (running) {
            val n = rec.read(buf, buf.capacity(), AudioRecord.READ_BLOCKING)
            if (n < 0) {
                if (running) onFailed("Recording failed (error $n)")
                break
            }
            val frames = Pcm.wholeFrames(n, channels)
            if (frames == 0) continue
            NativeBridge.onCaptureData(buf, frames * frameBytes)
            framesRead += frames
            if (++reads % LATENCY_EVERY == 0 &&
                rec.getTimestamp(ts, AudioTimestamp.TIMEBASE_MONOTONIC) == AudioRecord.SUCCESS
            ) {
                NativeBridge.onCaptureLatency(
                    Pcm.latencyMs(ts.framePosition, ts.nanoTime, framesRead, rate, System.nanoTime()),
                )
            }
        }
    }

    fun stop() {
        running = false
        record?.let { r ->
            try { r.stop() } catch (_: IllegalStateException) {}
        }
        thread?.join(500)
        thread = null
        record?.release()
        record = null
    }
}
