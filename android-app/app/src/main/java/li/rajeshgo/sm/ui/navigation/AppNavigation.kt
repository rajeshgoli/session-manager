package li.rajeshgo.sm.ui.navigation

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.compose.currentBackStackEntryAsState
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import androidx.compose.ui.platform.LocalContext
import androidx.navigation.NavType
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import androidx.navigation.navArgument
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.data.model.Reviewer
import li.rajeshgo.sm.data.model.CreateSessionRequest
import li.rajeshgo.sm.push.FollowOpen
import li.rajeshgo.sm.push.FollowOpenRequests
import li.rajeshgo.sm.ui.bug.BugReportViewModel
import li.rajeshgo.sm.ui.bug.BugScreen
import li.rajeshgo.sm.ui.bug.findActivity
import li.rajeshgo.sm.ui.reviews.StartReviewerRow
import li.rajeshgo.sm.ui.watch.BugStart
import li.rajeshgo.sm.ui.watch.CreateSessionSheet
import li.rajeshgo.sm.ui.analytics.AnalyticsScreen
import li.rajeshgo.sm.ui.board.BoardBadgeRefresher
import li.rajeshgo.sm.ui.board.BoardLinkRequests
import li.rajeshgo.sm.ui.board.BoardScreen
import li.rajeshgo.sm.ui.guestbook.GuestbookScreen
import li.rajeshgo.sm.ui.history.HistoryScreen
import li.rajeshgo.sm.ui.inbox.InboxBadgeRefresher
import li.rajeshgo.sm.ui.inbox.InboxScreen
import li.rajeshgo.sm.ui.notes.NotesScreen
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
    const val BOARD = "board"
    const val ANALYTICS = "analytics"
    const val QUEUE = "queue"
    const val USAGE = "usage"
    const val HISTORY = "history"
    const val NOTES = "notes"
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

    // Bottom tabs keep their state and view models while another tab shows (spec 1782 J5).
    // Watch is the root and never popped, so it has no saved state of its own: the pop
    // files the tab above it under Watch's id, and restoring that reopened the tab (sm#1924).
    val toTab = { route: String ->
        navController.navigate(route) {
            popUpTo(Routes.WATCH) { saveState = true }
            launchSingleTop = true
            restoreState = route != Routes.WATCH
        }
    }

    val pendingEnrollmentUrl = EnrollmentLinkRequests.pending
    LaunchedEffect(pendingEnrollmentUrl) {
        if (!pendingEnrollmentUrl.isNullOrBlank() && navController.currentDestination?.route != Routes.SETTINGS) {
            navController.navigate(Routes.SETTINGS) {
                launchSingleTop = true
            }
        }
    }

    // "GitHub Codex paused" and an Inbox item's "Change policy" open Settings › Reviews.
    val reviewSettingsAsked = ReviewSettingsRequests.pending
    LaunchedEffect(reviewSettingsAsked, isLoggedIn) {
        if (reviewSettingsAsked && isLoggedIn == true && navController.currentDestination?.route != Routes.SETTINGS) {
            navController.navigate(Routes.SETTINGS) { launchSingleTop = true }
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
            toTab(route)
        }
    }

    // The Inbox and Board badges on every screen's bottom nav, while the app is in front.
    val resumed = rememberResumed()
    val badgeRefresher = remember { InboxBadgeRefresher(context.applicationContext as android.app.Application) }
    val boardBadgeRefresher = remember { BoardBadgeRefresher(context.applicationContext as android.app.Application) }
    LaunchedEffect(resumed, isLoggedIn) {
        if (!resumed || isLoggedIn != true) return@LaunchedEffect
        while (isActive) {
            badgeRefresher.refresh()
            boardBadgeRefresher.refresh()
            delay(INBOX_BADGE_REFRESH_MS)
        }
    }
    val toInbox = { toTab(Routes.INBOX) }
    val toWatch = { toTab(Routes.WATCH) }
    val toBoard = { toTab(Routes.BOARD) }
    val toQueue = { toTab(Routes.QUEUE) }
    // The screen a bug report is about: its route and when it became visible.
    val backStackEntry by navController.currentBackStackEntryAsState()
    val currentRoute = backStackEntry?.destination?.route?.substringBefore('?')
    val section = backStackEntry?.arguments?.getString("section")
    LaunchedEffect(currentRoute, section) {
        currentRoute?.let { BugScreen.shown(it + section?.let { s -> "?section=$s" }.orEmpty()) }
    }
    val bugViewModel: BugReportViewModel = viewModel()
    val snackbars = remember { SnackbarHostState() }
    val uriHandler = LocalUriHandler.current
    LaunchedEffect(bugViewModel) {
        bugViewModel.toastFlow.collect { toast ->
            val tapped = snackbars.showSnackbar(toast.text, actionLabel = "Open", withDismissAction = true, duration = SnackbarDuration.Long)
            if (tapped == SnackbarResult.ActionPerformed) {
                val started = toast.started
                if (started != null) FollowOpenRequests.pending = FollowOpen(started.sessionId, null, started.name)
                else runCatching { uriHandler.openUri(toast.issueUrl) }
            }
        }
    }
    // The three-dots menu every screen shares (sm#1659).
    val menu = AppMenuActions(
        onNewSession = {
            NewSessionRequests.pending = true
            toWatch()
        },
        onOpenHistory = { navController.navigate(Routes.HISTORY) { launchSingleTop = true } },
        onOpenNotes = { navController.navigate(Routes.NOTES) { launchSingleTop = true } },
        onOpenGuestbook = { navController.navigate(Routes.GUESTBOOK) { launchSingleTop = true } },
        onOpenAnalytics = { navController.navigate(Routes.ANALYTICS) { launchSingleTop = true } },
        onOpenSettings = { navController.navigate(Routes.SETTINGS) { launchSingleTop = true } },
        onReportBug = { context.findActivity()?.let(bugViewModel::open) },
    )

    // An opened sm link opens in the watch screen's reader when signed in.
    val pendingReaderLink = ReaderLinkRequests.pending
    LaunchedEffect(pendingReaderLink, isLoggedIn) {
        if (pendingReaderLink != null && isLoggedIn == true &&
            navController.currentDestination?.route != Routes.WATCH
        ) {
            toTab(Routes.WATCH)
        }
    }

    // A `/board` link or a tapped board alert opens the Board tab, which scrolls to its lane.
    val pendingBoardLink = BoardLinkRequests.pending
    LaunchedEffect(pendingBoardLink, isLoggedIn) {
        if (pendingBoardLink != null && isLoggedIn == true &&
            navController.currentDestination?.route != Routes.BOARD
        ) {
            toBoard()
        }
    }

    Box(Modifier.fillMaxSize()) {
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
                    onNavigateToBoard = toBoard,
                    onNavigateToQueue = toQueue,
                    menu = menu,
                )
            }
            composable(Routes.WATCH) {
                WatchScreen(
                    onNavigateToInbox = toInbox,
                    onNavigateToBoard = toBoard,
                    onNavigateToQueue = toQueue,
                    menu = menu,
                )
            }
            composable(Routes.BOARD) {
                BoardScreen(
                    onNavigateToInbox = toInbox,
                    onNavigateToWatch = toWatch,
                    onNavigateToQueue = toQueue,
                    menu = menu,
                )
            }
            composable(Routes.QUEUE) {
                QueueScreen(
                    onNavigateToInbox = toInbox,
                    onNavigateToWatch = toWatch,
                    onNavigateToBoard = toBoard,
                    onOpenUsage = { navController.navigate(Routes.USAGE) },
                    onOpenStopped = {
                        navController.navigate("${Routes.ANALYTICS}?section=queue") { launchSingleTop = true }
                    },
                    menu = menu,
                )
            }
            composable(Routes.USAGE) {
                UsageScreen(onBack = { navController.popBackStack() })
            }
            composable(Routes.HISTORY) {
                HistoryScreen(onBack = { navController.popBackStack() }, onOpenWatch = toWatch, menu = menu)
            }
            composable(Routes.NOTES) { NotesScreen(onBack = { navController.popBackStack() }, menu = menu) }
            composable(Routes.GUESTBOOK) {
                GuestbookScreen(onBack = { navController.popBackStack() }, menu = menu)
            }
            // `section` (spend, time or queue) is optional; without it the last one shown opens.
            composable(
                "${Routes.ANALYTICS}?section={section}",
                arguments = listOf(navArgument("section") { type = NavType.StringType; nullable = true }),
            ) { backStackEntry ->
                AnalyticsScreen(
                    section = backStackEntry.arguments?.getString("section"),
                    onBack = { navController.popBackStack() },
                    onOpenUsage = { navController.navigate(Routes.USAGE) },
                    onOpenWatch = toWatch,
                    menu = menu,
                )
            }
        }
        BugReportSheet(bugViewModel)
        // Above the bottom tabs, so a filed bug's toast does not cover them.
        SnackbarHost(snackbars, Modifier.align(Alignment.BottomCenter).navigationBarsPadding().padding(bottom = 72.dp))
    }
}

/** Report a bug: the Start sheet in its bug mode, with Start's Reviewer row (spec 1859 C4). */
@Composable
private fun BugReportSheet(viewModel: BugReportViewModel) {
    val sheet = viewModel.sheet ?: return
    val options = sheet.options
    val defaults = sheet.defaults
    var reviewer by remember(options?.reviewPolicy) { mutableStateOf<Reviewer?>(null) }
    CreateSessionSheet(
        source = null,
        sessions = emptyList(),
        loadModels = viewModel::sessionModels,
        busy = sheet.busy,
        error = sheet.error,
        onDismiss = viewModel::close,
        bug = BugStart(
            defaults = if (options != null && defaults != null && !options.workingDir.isNullOrBlank()) {
                CreateSessionRequest(defaults.provider, options.workingDir, defaults.model, defaults.effort)
            } else null,
            agentNote = sheet.agentError ?: options?.takeIf { it.workingDir.isNullOrBlank() }?.let { "No checkout of ${it.repo} to start an agent in" },
            screenshot = sheet.thumbnail,
            filedIssue = sheet.filedIssue,
        ),
        extra = { enabled ->
            StartReviewerRow(laneDefault = options?.reviewPolicy, value = reviewer, onChange = { reviewer = it }, enabled = enabled)
        },
        onFile = { filing -> viewModel.submit(filing.copy(start = filing.start?.copy(reviewer = reviewer))) },
        onCreate = {},
    )
}
