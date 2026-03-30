package li.rajeshgo.sm.ui.navigation

import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.platform.LocalContext
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.analytics.AnalyticsDetailScreen
import li.rajeshgo.sm.ui.analytics.AnalyticsScreen
import li.rajeshgo.sm.ui.settings.SettingsScreen
import li.rajeshgo.sm.ui.watch.WatchScreen

object Routes {
    const val SETTINGS = "settings"
    const val WATCH = "watch"
    const val ANALYTICS = "analytics"
    const val ANALYTICS_DETAIL = "analytics/detail"
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
                onNavigateToAnalytics = {
                    navController.navigate(Routes.ANALYTICS) {
                        launchSingleTop = true
                    }
                },
            )
        }
        composable(Routes.ANALYTICS) {
            AnalyticsScreen(
                onNavigateToWatch = {
                    navController.navigate(Routes.WATCH) {
                        popUpTo(Routes.WATCH) { inclusive = false }
                        launchSingleTop = true
                    }
                },
                onNavigateToSettings = {
                    navController.navigate(Routes.SETTINGS)
                },
                onOpenDetail = { section ->
                    navController.navigate("${Routes.ANALYTICS_DETAIL}/$section")
                },
            )
        }
        composable("${Routes.ANALYTICS_DETAIL}/{section}") { backStackEntry ->
            AnalyticsDetailScreen(
                section = backStackEntry.arguments?.getString("section").orEmpty(),
                onBack = { navController.popBackStack() },
                onNavigateToWatch = {
                    navController.navigate(Routes.WATCH) {
                        popUpTo(Routes.WATCH) { inclusive = false }
                        launchSingleTop = true
                    }
                },
                onNavigateToAnalytics = {
                    navController.navigate(Routes.ANALYTICS) {
                        popUpTo(Routes.ANALYTICS) { inclusive = false }
                        launchSingleTop = true
                    }
                },
                onNavigateToSettings = {
                    navController.navigate(Routes.SETTINGS)
                },
            )
        }
    }
}
