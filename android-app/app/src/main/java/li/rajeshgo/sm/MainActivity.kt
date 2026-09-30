package li.rajeshgo.sm

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import li.rajeshgo.sm.push.FollowOpen
import li.rajeshgo.sm.push.FollowOpenRequests
import li.rajeshgo.sm.ui.board.BoardLinkRequests
import li.rajeshgo.sm.ui.board.boardLaneForLink
import li.rajeshgo.sm.ui.board.boardLaneFromPath
import li.rajeshgo.sm.ui.navigation.AppNavigation
import li.rajeshgo.sm.ui.navigation.EnrollmentLinkRequests
import li.rajeshgo.sm.ui.navigation.ReaderLinkRequests
import li.rajeshgo.sm.ui.theme.SessionManagerTheme
import li.rajeshgo.sm.ui.watch.readerPageForLink

class MainActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // A fresh activity owns the requests: a launcher start drops any left by
        // an abandoned notification tap or link (the process can outlive the activity).
        FollowOpenRequests.pending = null
        EnrollmentLinkRequests.pending = null
        ReaderLinkRequests.pending = null
        BoardLinkRequests.pending = null
        takeEnrollmentLink(intent)
        takeReaderLink(intent)
        takeFollowOpen(intent)
        enableEdgeToEdge(
            statusBarStyle = androidx.activity.SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = androidx.activity.SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
        )
        setContent {
            SessionManagerTheme {
                androidx.compose.material3.Surface(color = androidx.compose.material3.MaterialTheme.colorScheme.background) {
                    AppNavigation()
                }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        takeEnrollmentLink(intent)
        takeReaderLink(intent)
        takeFollowOpen(intent)
    }

    private fun enrollmentUrlFromIntent(intent: Intent?): String? {
        val uri = intent?.data ?: return null
        if (uri.scheme != "sm-enroll" || uri.host != "enroll") {
            return null
        }
        return uri.getQueryParameter("url")?.trim()?.takeIf { it.isNotBlank() }
    }

    /** Hands a tapped follow notification to the watch screen, and a board alert to the Board tab, once. */
    private fun takeFollowOpen(intent: Intent?) {
        val open = FollowOpen.fromIntent(intent) ?: return
        val lane = open.readerPath?.let(::boardLaneFromPath)
        if (lane != null) BoardLinkRequests.pending = lane else FollowOpenRequests.pending = open
        setIntent(Intent(this, MainActivity::class.java))
    }

    /** Hands an opened sm link (doc, ticket page, History) to the watch screen's reader, once. */
    private fun takeReaderLink(intent: Intent?) {
        if (intent?.action != Intent.ACTION_VIEW) return
        val url = intent.dataString ?: return
        boardLaneForLink(url, BuildConfig.SM_LINK_HOST)?.let { lane ->
            BoardLinkRequests.pending = lane
            setIntent(Intent(this, MainActivity::class.java))
            return
        }
        val page = readerPageForLink(url, BuildConfig.SM_LINK_HOST) ?: return
        ReaderLinkRequests.pending = page
        setIntent(Intent(this, MainActivity::class.java))
    }

    /** Hands an opened enrollment link to the Settings screen, once. */
    private fun takeEnrollmentLink(intent: Intent?) {
        val url = enrollmentUrlFromIntent(intent) ?: return
        EnrollmentLinkRequests.pending = url
        setIntent(Intent(this, MainActivity::class.java))
    }
}
