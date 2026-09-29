package li.rajeshgo.sm.debug

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch

/** Owns one enrollment run across Activity recreation. Blocks must not capture an Activity. */
class AndroidSmokeViewModel : ViewModel() {
    private var run: Job? = null

    // Called on the main thread by onCreate. A finished run is also retained:
    // a recreated Activity observes completion instead of reusing the token.
    fun start(block: suspend () -> Unit): Job =
        run ?: viewModelScope.launch { block() }.also { run = it }
}
