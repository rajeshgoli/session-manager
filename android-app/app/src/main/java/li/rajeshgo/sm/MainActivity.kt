package li.rajeshgo.sm

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import li.rajeshgo.sm.ui.navigation.AppNavigation
import li.rajeshgo.sm.ui.theme.SessionManagerTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            SessionManagerTheme {
                AppNavigation()
            }
        }
    }
}
