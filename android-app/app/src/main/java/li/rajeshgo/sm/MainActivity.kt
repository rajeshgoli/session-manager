package li.rajeshgo.sm

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.mutableStateOf
import li.rajeshgo.sm.push.FollowOpen
import li.rajeshgo.sm.push.FollowOpenRequests
import li.rajeshgo.sm.ui.navigation.AppNavigation
import li.rajeshgo.sm.ui.theme.SessionManagerTheme

class MainActivity : ComponentActivity() {
    private val pendingEnrollmentUrl = mutableStateOf<String?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        pendingEnrollmentUrl.value = enrollmentUrlFromIntent(intent)
        // A fresh activity owns the request: a launcher start drops one left by
        // an abandoned notification tap (the process can outlive the activity).
        FollowOpenRequests.pending = null
        takeFollowOpen(intent)
        enableEdgeToEdge(
            statusBarStyle = androidx.activity.SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = androidx.activity.SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
        )
        setContent {
            SessionManagerTheme {
                androidx.compose.material3.Surface(color = androidx.compose.material3.MaterialTheme.colorScheme.background) {
                    AppNavigation(
                        pendingEnrollmentUrl = pendingEnrollmentUrl.value,
                        onEnrollmentDeepLinkConsumed = ::clearEnrollmentDeepLink,
                    )
                }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        pendingEnrollmentUrl.value = enrollmentUrlFromIntent(intent)
        takeFollowOpen(intent)
    }

    private fun enrollmentUrlFromIntent(intent: Intent?): String? {
        val uri = intent?.data ?: return null
        if (uri.scheme != "sm-enroll" || uri.host != "enroll") {
            return null
        }
        return uri.getQueryParameter("url")?.trim()?.takeIf { it.isNotBlank() }
    }

    /** Hands a tapped follow notification to the watch screen, once. */
    private fun takeFollowOpen(intent: Intent?) {
        val open = FollowOpen.fromIntent(intent) ?: return
        FollowOpenRequests.pending = open
        setIntent(Intent(this, MainActivity::class.java))
    }

    private fun clearEnrollmentDeepLink() {
        pendingEnrollmentUrl.value = null
        setIntent(Intent(this, MainActivity::class.java))
    }
}
