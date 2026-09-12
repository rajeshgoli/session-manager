package li.rajeshgo.sm.ui.watch

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/** Keep fixed control keys independent of renderer initialization and failures. */
internal class TerminalControls {
    var rendererReady by mutableStateOf(false)
    var sendArrow: ((String) -> Unit)? = null

    fun canSend(key: String): Boolean = key in fixedKeys ||
        (key in arrowKeys && rendererReady && sendArrow != null)

    fun sendKey(key: String, sendInput: (String) -> Unit) {
        val sequence = fixedKeys[key]
        if (sequence != null) {
            sendInput(sequence)
        } else if (canSend(key)) {
            // Only arrows depend on the cursor mode tracked by xterm.
            sendArrow?.invoke(key)
        }
    }

    private companion object {
        val arrowKeys = setOf("up", "down", "left", "right")
        val fixedKeys = mapOf(
            "enter" to "\r", "esc" to "\u001b", "tab" to "\t",
            "shift-tab" to "\u001b[Z", "backspace" to "\u007f", "ctrl-c" to "\u0003",
        )
    }
}
