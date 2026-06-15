package li.rajeshgo.sm

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.mutableStateOf
import li.rajeshgo.sm.ui.navigation.AppNavigation
import li.rajeshgo.sm.ui.theme.SessionManagerTheme

class MainActivity : ComponentActivity() {
    private val pendingEnrollmentUrl = mutableStateOf<String?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        pendingEnrollmentUrl.value = enrollmentUrlFromIntent(intent)
        enableEdgeToEdge()
        setContent {
            SessionManagerTheme {
                AppNavigation(
                    pendingEnrollmentUrl = pendingEnrollmentUrl.value,
                    onEnrollmentDeepLinkConsumed = {
                        pendingEnrollmentUrl.value = null
                    },
                )
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        pendingEnrollmentUrl.value = enrollmentUrlFromIntent(intent)
    }

    private fun enrollmentUrlFromIntent(intent: Intent?): String? {
        val uri = intent?.data ?: return null
        if (uri.scheme != "sm-enroll" || uri.host != "enroll") {
            return null
        }
        return uri.getQueryParameter("url")?.trim()?.takeIf { it.isNotBlank() }
    }
}
