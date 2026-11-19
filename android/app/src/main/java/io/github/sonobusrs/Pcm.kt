package io.github.sonobusrs

/** Pure helpers of the capture thread (unit-tested on the JVM). */
object Pcm {
    /** Bytes per interleaved float frame. */
    fun frameBytes(channels: Int) = channels * 4

    /** Whole frames in [bytes]; a read that is not frame aligned would shift channels. */
    fun wholeFrames(bytes: Int, channels: Int) = bytes / frameBytes(channels)

    /**
     * Age in ms of the newest frame read so far: frame [framePosition] was captured at
     * [frameNanos] (an `AudioTimestamp`, monotonic), [framesRead] frames have been read, and
     * it is now [nowNanos].
     */
    fun latencyMs(framePosition: Long, frameNanos: Long, framesRead: Long, sampleRate: Int, nowNanos: Long): Float {
        val newestNanos = frameNanos + (framesRead - framePosition) * 1_000_000_000L / sampleRate
        return (nowNanos - newestNanos) / 1e6f
    }
}
