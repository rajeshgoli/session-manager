package li.rajeshgo.sm.ui.watch

/** Bounds both posted callbacks and frames awaiting xterm, including legacy servers. */
internal class TerminalOutputBudget {
    private var frames = 0
    private var chars = 0
    private var exceeded = false

    @Synchronized
    fun reserve(length: Int): Boolean {
        if (exceeded || frames >= 64 || length > 1_048_576 - chars) {
            exceeded = true
            return false
        }
        frames++
        chars += length
        return true
    }

    @Synchronized
    fun release(length: Int) {
        frames--
        chars -= length
    }
}
