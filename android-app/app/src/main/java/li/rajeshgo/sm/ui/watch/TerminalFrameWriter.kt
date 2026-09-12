package li.rajeshgo.sm.ui.watch

/** Keeps renderer events out of OkHttp's pre-open queue. All calls use the main thread. */
internal class TerminalFrameWriter {
    private var sender: ((String) -> Boolean)? = null

    fun authenticate(auth: String, send: (String) -> Boolean): Boolean {
        if (sender != null || !send(auth)) return false
        sender = send
        return true
    }

    fun send(frame: String): Boolean = sender?.invoke(frame) ?: false
}

internal fun retryTerminalClose(code: Int, reason: String): Boolean =
    reason != "tmux_session_closed" && (code != 1008 ||
        reason.equals("First terminal frame must be auth", ignoreCase = true))

internal fun terminalCloseMessage(reason: String): String = when {
    reason == "tmux_session_closed" -> "This session has ended. You can close this window."
    reason.contains("revok", ignoreCase = true) -> "Device access was revoked. Check device enrollment in Settings."
    else -> "Couldn't verify this connection. Retry, or check device enrollment in Settings. Your draft is saved."
}
