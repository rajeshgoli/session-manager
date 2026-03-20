package li.rajeshgo.sm.ui.navigation

import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.settings.SettingsScreen
import li.rajeshgo.sm.ui.watch.WatchScreen

object Routes {
    const val SETTINGS = "settings"
    const val WATCH = "watch"
}

@Composable
fun AppNavigation() {
    val navController = rememberNavController()
    val context = LocalContext.current
    val settingsRepository = SettingsRepository(context)
    val isLoggedIn by settingsRepository.isLoggedIn.collectAsState(initial = null)

    val startDestination = when (isLoggedIn) {
        true -> Routes.WATCH
        false -> Routes.SETTINGS
        null -> return
    }

    NavHost(navController = navController, startDestination = startDestination) {
        composable(Routes.SETTINGS) {
            SettingsScreen(
                onNavigateToWatch = {
                    navController.navigate(Routes.WATCH) {
                        popUpTo(Routes.SETTINGS) { inclusive = true }
                    }
                },
            )
        }
        composable(Routes.WATCH) {
            WatchScreen(
                onNavigateToSettings = {
                    navController.navigate(Routes.SETTINGS)
                },
            )
        }
    }
}
