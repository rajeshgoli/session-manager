package li.rajeshgo.sm.ui.watch

import org.junit.Assert.*
import org.junit.Test

class TerminalControlsTest {
    @Test fun fixedKeysWorkBeforeRendererInitializationAndAfterFailure() {
        val controls = TerminalControls()
        val sent = mutableListOf<String>()
        val keys = listOf("enter", "esc", "tab", "shift-tab", "backspace", "ctrl-c")
        val expected = listOf("\r", "\u001b", "\t", "\u001b[Z", "\u007f", "\u0003")
        fun pressAll() {
            keys.forEach { key ->
                assertTrue(controls.canSend(key))
                controls.sendKey(key) { sent.add(it) }
            }
        }
        pressAll()
        assertEquals(expected, sent)
        controls.sendArrow = { fail("Fixed keys must not depend on WebView") }
        controls.rendererReady = true
        controls.rendererReady = false
        sent.clear()
        pressAll()
        assertEquals(expected, sent)
    }

    @Test fun arrowsRequireAReadyRendererAndRecoverAfterRendererFailure() {
        val controls = TerminalControls()
        val sent = mutableListOf<String>()
        val arrows = listOf("up", "down", "left", "right")
        fun pressAll() = arrows.forEach { controls.sendKey(it) { fail("Arrow must use xterm cursor mode") } }
        controls.sendArrow = { sent.add(it) }
        assertFalse(controls.canSend("up"))
        pressAll()
        assertTrue(sent.isEmpty())
        controls.rendererReady = true
        assertTrue(controls.canSend("up"))
        pressAll()
        assertEquals(arrows, sent)
        controls.rendererReady = false
        assertFalse(controls.canSend("up"))
        pressAll()
        assertEquals(arrows, sent)
        controls.rendererReady = true
        pressAll()
        assertEquals(arrows + arrows, sent)
        controls.sendArrow = null
        assertFalse(controls.canSend("up"))
        assertFalse(controls.canSend("unknown"))
    }
}
