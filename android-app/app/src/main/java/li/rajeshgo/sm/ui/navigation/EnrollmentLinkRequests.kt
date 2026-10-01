package li.rajeshgo.sm.ui.navigation

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import li.rajeshgo.sm.ui.watch.ReaderPage

/**
 * An opened `sm-enroll://enroll?url=…` link waiting for the Settings screen.
 * Process-wide Compose state, read directly by the screens for the same
 * reason as FollowOpenRequests: a value handed through the NavHost builder is
 * captured when the graph is built, so a link opened while the app is already
 * running would never reach Settings.
 */
object EnrollmentLinkRequests {
    var pending by mutableStateOf<String?>(null)
}

/** An opened sm link (a doc, ticket page or History) waiting for the watch screen's reader. */
object ReaderLinkRequests {
    var pending by mutableStateOf<ReaderPage?>(null)
}

/** New session chosen from a screen's menu, waiting for the watch screen's create sheet (sm#1659). */
object NewSessionRequests {
    var pending by mutableStateOf(false)
}

/** Settings › Reviews asked for, by a "GitHub Codex paused" tap or "Change policy" (sm#1768 G5, D7). */
object ReviewSettingsRequests {
    var pending by mutableStateOf(false)
}
