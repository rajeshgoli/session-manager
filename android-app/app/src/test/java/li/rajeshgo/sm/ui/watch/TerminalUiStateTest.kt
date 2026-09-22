package li.rajeshgo.sm.ui.watch

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class TerminalUiStateTest {
    @Test
    fun slowRendererKeepsEntireBurstUntilAcknowledged() {
        var state = TerminalUiState("test", "Test")
        val frames = (1L..1200L).map {
            TerminalOutputFrame(it, if (it == 1L) "\u001b[2J\u001b[H" else "line $it\r\n")
        }
        frames.forEach { state = state.enqueueOutput(it) }
        assertEquals(frames, state.outputFrames)

        state = state.acknowledgeOutput(800)
        assertEquals(frames.drop(800), state.outputFrames)
        state = state.acknowledgeOutput(400) // delayed older acknowledgement
        assertEquals(800L, state.rendererLastAckSequence)
        assertEquals(frames.drop(800), state.outputFrames)
        state = state.acknowledgeOutput(1200)
        assertTrue(state.outputFrames.isEmpty())
        assertEquals(1200L, state.outputSequence)
    }
}
