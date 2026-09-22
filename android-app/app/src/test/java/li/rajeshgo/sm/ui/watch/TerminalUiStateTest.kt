package li.rajeshgo.sm.ui.watch

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class TerminalUiStateTest {
    @Test
    fun rendererAcknowledgementsReleaseEachWindowWithoutLosingFrames() {
        var state = TerminalUiState("test", "Test")
        val budget = TerminalOutputBudget()
        for (window in 0..99) {
            val frames = (1L..32L).map {
                TerminalOutputFrame(window * 32L + it, "line $it\r\n")
            }
            frames.forEach {
                assertTrue(budget.reserve(it.data.length))
                state = state.enqueueOutput(it)
            }
            assertEquals(frames, state.outputFrames)
            frames.forEach { budget.release(it.data.length) }
            state = state.acknowledgeOutput(frames.last().sequence)
            state = state.acknowledgeOutput(frames.first().sequence)
            assertTrue(state.outputFrames.isEmpty())
            assertEquals(frames.last().sequence, state.rendererLastAckSequence)
        }
    }

    @Test
    fun stalledLegacyRendererHasBoundedCallbacksAndMemory() {
        val frames = TerminalOutputBudget()
        repeat(64) { assertTrue(frames.reserve(10)) }
        assertFalse(frames.reserve(10))
        frames.release(10)
        assertFalse(frames.reserve(10))
        val bytes = TerminalOutputBudget()
        assertTrue(bytes.reserve(1_048_576))
        assertFalse(bytes.reserve(1))
        assertFalse(TerminalOutputBudget().reserve(1_048_577))
    }
}
