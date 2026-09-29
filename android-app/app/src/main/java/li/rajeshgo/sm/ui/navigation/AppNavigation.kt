package li.rajeshgo.sm.ui.navigation

import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import androidx.compose.ui.platform.LocalContext
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.push.FollowOpenRequests
import li.rajeshgo.sm.ui.analytics.AnalyticsDetailScreen
import li.rajeshgo.sm.ui.analytics.AnalyticsScreen
import li.rajeshgo.sm.ui.guestbook.GuestbookScreen
import li.rajeshgo.sm.ui.history.HistoryScreen
import li.rajeshgo.sm.ui.inbox.InboxBadgeRefresher
import li.rajeshgo.sm.ui.inbox.InboxScreen
import li.rajeshgo.sm.ui.queue.rememberResumed
import li.rajeshgo.sm.ui.queue.QueueScreen
import li.rajeshgo.sm.ui.queue.UsageScreen
import li.rajeshgo.sm.ui.settings.SettingsScreen
import li.rajeshgo.sm.ui.watch.WatchScreen

private const val INBOX_BADGE_REFRESH_MS = 60_000L

object Routes {
    const val SETTINGS = "settings"
    const val WATCH = "watch"
    const val INBOX = "inbox"
    const val ANALYTICS = "analytics"
    const val ANALYTICS_DETAIL = "analytics/detail"
    const val QUEUE = "queue"
    const val USAGE = "usage"
    const val HISTORY = "history"
    const val GUESTBOOK = "guestbook"
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

    val pendingEnrollmentUrl = EnrollmentLinkRequests.pending
    LaunchedEffect(pendingEnrollmentUrl) {
        if (!pendingEnrollmentUrl.isNullOrBlank() && navController.currentDestination?.route != Routes.SETTINGS) {
            navController.navigate(Routes.SETTINGS) {
                launchSingleTop = true
            }
        }
    }

    // A tapped notification opens when signed in: messages and review
    // requests in the Inbox (sm#1647), follow results on the watch screen.
    val pendingFollowOpen = FollowOpenRequests.pending
    LaunchedEffect(pendingFollowOpen, isLoggedIn) {
        val route = if (pendingFollowOpen?.inbox == true) Routes.INBOX else Routes.WATCH
        if (pendingFollowOpen != null && isLoggedIn == true &&
            navController.currentDestination?.route != route
        ) {
            navController.navigate(route) {
                launchSingleTop = true
            }
        }
    }

    // The Inbox badge on every screen's bottom nav, while the app is in front.
    val resumed = rememberResumed()
    val badgeRefresher = remember { InboxBadgeRefresher(context.applicationContext as android.app.Application) }
    LaunchedEffect(resumed, isLoggedIn) {
        if (!resumed || isLoggedIn != true) return@LaunchedEffect
        while (isActive) {
            badgeRefresher.refresh()
            delay(INBOX_BADGE_REFRESH_MS)
        }
    }
    val toInbox = {
        navController.navigate(Routes.INBOX) {
            popUpTo(Routes.WATCH) { inclusive = false }
            launchSingleTop = true
        }
    }

    val toWatch = {
        navController.navigate(Routes.WATCH) {
            popUpTo(Routes.WATCH) { inclusive = false }
            launchSingleTop = true
        }
    }
    val toQueue = {
        navController.navigate(Routes.QUEUE) {
            popUpTo(Routes.WATCH) { inclusive = false }
            launchSingleTop = true
        }
    }
    // The three-dots menu every screen shares (sm#1659).
    val menu = AppMenuActions(
        onNewSession = {
            NewSessionRequests.pending = true
            toWatch()
        },
        onOpenHistory = { navController.navigate(Routes.HISTORY) { launchSingleTop = true } },
        onOpenGuestbook = { navController.navigate(Routes.GUESTBOOK) { launchSingleTop = true } },
        onOpenAnalytics = { navController.navigate(Routes.ANALYTICS) { launchSingleTop = true } },
        onOpenSettings = { navController.navigate(Routes.SETTINGS) { launchSingleTop = true } },
    )

    // An opened sm link opens in the watch screen's reader when signed in.
    val pendingReaderLink = ReaderLinkRequests.pending
    LaunchedEffect(pendingReaderLink, isLoggedIn) {
        if (pendingReaderLink != null && isLoggedIn == true &&
            navController.currentDestination?.route != Routes.WATCH
        ) {
            navController.navigate(Routes.WATCH) {
                launchSingleTop = true
            }
        }
    }

    NavHost(navController = navController, startDestination = startDestination) {
        composable(Routes.SETTINGS) {
            SettingsScreen(
                onNavigateToWatch = {
                    if (!navController.popBackStack()) navController.navigate(Routes.WATCH) {
                        popUpTo(Routes.SETTINGS) { inclusive = true }
                    }
                },
            )
        }
        composable(Routes.INBOX) {
            InboxScreen(
                onNavigateToWatch = toWatch,
                onNavigateToQueue = toQueue,
                menu = menu,
            )
        }
        composable(Routes.WATCH) {
            WatchScreen(
                onNavigateToInbox = toInbox,
                onNavigateToQueue = toQueue,
                menu = menu,
            )
        }
        composable(Routes.QUEUE) {
            QueueScreen(
                onNavigateToInbox = toInbox,
                onNavigateToWatch = toWatch,
                onOpenUsage = { navController.navigate(Routes.USAGE) },
                menu = menu,
            )
        }
        composable(Routes.USAGE) {
            UsageScreen(onBack = { navController.popBackStack() })
        }
        composable(Routes.HISTORY) {
            HistoryScreen(onBack = { navController.popBackStack() }, onOpenWatch = toWatch, menu = menu)
        }
        composable(Routes.GUESTBOOK) {
            GuestbookScreen(onBack = { navController.popBackStack() }, menu = menu)
        }
        composable(Routes.ANALYTICS) {
            AnalyticsScreen(
                onBack = { navController.popBackStack() },
                menu = menu,
                onOpenDetail = { section ->
                    navController.navigate("${Routes.ANALYTICS_DETAIL}/$section")
                },
            )
        }
        composable("${Routes.ANALYTICS_DETAIL}/{section}") { backStackEntry ->
            AnalyticsDetailScreen(
                section = backStackEntry.arguments?.getString("section").orEmpty(),
                onBack = { navController.popBackStack() },
                menu = menu,
            )
        }
    }
}
