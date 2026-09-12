package li.rajeshgo.sm.ui.watch

import org.junit.Assert.*
import org.junit.Test

class TerminalFrameWriterTest {
    @Test fun slowHandshakeCannotQueueRendererOrKeyboardFramesBeforeAuth() {
        val frames = mutableListOf<String>()
        val writer = TerminalFrameWriter()
        assertFalse(writer.send("resize"))
        assertFalse(writer.send("input"))
        assertFalse(writer.send("key"))
        assertTrue(writer.authenticate("auth") { frames.add(it) })
        assertTrue(writer.send("resize"))
        assertTrue(writer.send("input"))
        assertEquals(listOf("auth", "resize", "input"), frames)
    }

    @Test fun rejectedAuthAndReplacementStayClosedUntilTheirOwnHandshake() {
        val writer = TerminalFrameWriter()
        assertFalse(writer.authenticate("auth") { false })
        assertFalse(writer.send("input"))
        assertTrue(writer.authenticate("auth") { true })
        assertFalse(writer.authenticate("auth") { fail("Must not authenticate twice"); true })
        assertFalse(TerminalFrameWriter().send("resize"))
    }

    @Test fun protocolOrderingFailureRecoversButAccessDenialAndEndedSessionsDoNot() {
        assertTrue(retryTerminalClose(1008, "First terminal frame must be auth"))
        assertTrue(retryTerminalClose(1000, "max_attach_seconds"))
        assertFalse(retryTerminalClose(1008, "device_revoked"))
        assertFalse(retryTerminalClose(1000, "tmux_session_closed"))
        assertFalse(terminalCloseMessage("First terminal frame must be auth").contains("frame"))
    }
}
