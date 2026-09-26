package li.rajeshgo.sm.ui.navigation

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

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
