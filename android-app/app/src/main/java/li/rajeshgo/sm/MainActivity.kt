package li.rajeshgo.sm

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.mutableStateOf
import li.rajeshgo.sm.push.FollowOpen
import li.rajeshgo.sm.ui.navigation.AppNavigation
import li.rajeshgo.sm.ui.theme.SessionManagerTheme

class MainActivity : ComponentActivity() {
    private val pendingEnrollmentUrl = mutableStateOf<String?>(null)
    private val pendingFollowOpen = mutableStateOf<FollowOpen?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        pendingEnrollmentUrl.value = enrollmentUrlFromIntent(intent)
        pendingFollowOpen.value = FollowOpen.fromIntent(intent)
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
                        pendingFollowOpen = pendingFollowOpen.value,
                        onFollowOpenConsumed = ::clearFollowOpen,
                    )
                }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        pendingEnrollmentUrl.value = enrollmentUrlFromIntent(intent)
        FollowOpen.fromIntent(intent)?.let { pendingFollowOpen.value = it }
    }

    private fun enrollmentUrlFromIntent(intent: Intent?): String? {
        val uri = intent?.data ?: return null
        if (uri.scheme != "sm-enroll" || uri.host != "enroll") {
            return null
        }
        return uri.getQueryParameter("url")?.trim()?.takeIf { it.isNotBlank() }
    }

    private fun clearFollowOpen() {
        pendingFollowOpen.value = null
        setIntent(Intent(this, MainActivity::class.java))
    }

    private fun clearEnrollmentDeepLink() {
        pendingEnrollmentUrl.value = null
        setIntent(Intent(this, MainActivity::class.java))
    }
}
