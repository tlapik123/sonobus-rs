package io.github.sonobusrs

import org.junit.Assert.assertEquals
import org.junit.Test

class PcmTest {
    @Test
    fun framing() {
        assertEquals(8, Pcm.frameBytes(2))
        assertEquals(240, Pcm.wholeFrames(1920, 2))
        // a read cut mid-frame never hands over the partial frame
        assertEquals(239, Pcm.wholeFrames(1919, 2))
        assertEquals(0, Pcm.wholeFrames(7, 2))
        assertEquals(160, Pcm.wholeFrames(1920, 3))
    }

    @Test
    fun latencyFromTimestamp() {
        // frame 48000 was captured at t = 10 s; 48240 frames read (newest captured 5 ms later)
        val t0 = 10_000_000_000L
        val ms = Pcm.latencyMs(48_000, t0, 48_240, 48_000, t0 + 25_000_000L)
        assertEquals(20.0f, ms, 0.001f)
        // timestamp newer than the reads (recorder ahead of us): data waiting in its buffer
        assertEquals(30.0f, Pcm.latencyMs(48_480, t0, 48_000, 48_000, t0 + 20_000_000L), 0.001f)
    }
}
