package li.rajeshgo.sm.ui.watch

import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.imePadding
import androidx.compose.runtime.key
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import android.annotation.SuppressLint
import android.os.Handler
import android.os.Looper
import android.webkit.JavascriptInterface
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import android.content.Intent
import android.net.Uri
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectVerticalDragGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.OpenInNew
import androidx.compose.material.icons.rounded.ContentCopy
import androidx.compose.material.icons.rounded.Campaign
import androidx.compose.material.icons.rounded.History
import androidx.compose.material.icons.rounded.MoreVert
import androidx.compose.material.icons.rounded.Notifications
import androidx.compose.material.icons.rounded.NotificationsActive
import androidx.compose.material.icons.rounded.NotificationsNone
import androidx.compose.material.icons.rounded.QuestionAnswer
import androidx.compose.material.icons.rounded.Refresh
import androidx.compose.material.icons.rounded.RestartAlt
import androidx.compose.material.icons.rounded.Add
import androidx.compose.material.icons.rounded.SupportAgent
import androidx.compose.material.icons.rounded.Terminal
import androidx.compose.material.icons.rounded.UnfoldLess
import androidx.compose.material.icons.rounded.UnfoldMore
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.AssistChip
import androidx.compose.material3.AssistChipDefaults
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.material.icons.rounded.MoreHoriz
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlin.coroutines.coroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import org.json.JSONObject
import java.io.ByteArrayInputStream
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.SessionDetail
import li.rajeshgo.sm.data.model.SessionClaim
import li.rajeshgo.sm.data.model.SessionDoc
import li.rajeshgo.sm.data.model.SessionJob
import li.rajeshgo.sm.ui.navigation.AppBottomNav
import li.rajeshgo.sm.ui.navigation.Routes
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.BorderStrong
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Emerald
import li.rajeshgo.sm.ui.theme.Fuchsia
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.PanelElevated
import li.rajeshgo.sm.ui.theme.PanelMuted
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.theme.Violet
import li.rajeshgo.sm.ui.update.SettingsIconButtonWithUpdate
import li.rajeshgo.sm.ui.update.UpdateAvailabilityViewModel
import li.rajeshgo.sm.ui.update.UpdateReadyBanner
import li.rajeshgo.sm.util.launchTermuxAttach
import li.rajeshgo.sm.util.termuxAttachCommand

private const val WATCH_AUTO_REFRESH_MS = 5000L
private const val WATCH_TOAST_MS = 3500L

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun WatchScreen(
    onNavigateToSettings: () -> Unit,
    onNavigateToAnalytics: () -> Unit,
    viewModel: WatchViewModel = viewModel(),
    updateViewModel: UpdateAvailabilityViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val updateState by updateViewModel.uiState.collectAsState()
    var creating by remember { mutableStateOf(false) }
    var cloneSource by remember { mutableStateOf<ClientSession?>(null) }
    var createBusy by remember { mutableStateOf(false) }
    var createError by remember { mutableStateOf<String?>(null) }
    var query by remember { mutableStateOf("") }
    var filter by remember { mutableStateOf("all") }
    var toast by remember { mutableStateOf<String?>(null) }
    var openPage by remember { mutableStateOf<ReaderPage?>(null) }
    var followDialogSession by remember { mutableStateOf<ClientSession?>(null) }
    var pendingNotificationAction by remember { mutableStateOf<(() -> Unit)?>(null) }

    val sections = remember(state.sessions, filter, query) {
        filterSections(buildSections(state.sessions), filter, query)
    }
    val activeSections = remember(sections) { sliceSections(sections, TreeSlice.Active) }
    val idleSections = remember(sections) { sliceSections(sections, TreeSlice.Idle) }
    val sessionsById = remember(state.sessions) { state.sessions.associateBy { it.id } }
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    var isResumed by remember {
        mutableStateOf(lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED))
    }
    val notificationPermission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (!granted) toast = NOTIFICATIONS_OFF_MESSAGE
        pendingNotificationAction?.invoke()
        pendingNotificationAction = null
    }
    // Android 13+ needs permission to show notifications; ask on the first follow and follow either way.
    val withNotificationPermission: (() -> Unit) -> Unit = { action ->
        if (android.os.Build.VERSION.SDK_INT >= 33 &&
            androidx.core.content.ContextCompat.checkSelfPermission(context, android.Manifest.permission.POST_NOTIFICATIONS) !=
            android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            pendingNotificationAction = action
            notificationPermission.launch(android.Manifest.permission.POST_NOTIFICATIONS)
        } else {
            action()
        }
    }
    val follow = FollowUi(
        followedSessionIds = state.followedSessionIds,
        followedJobIds = state.followedJobIds,
        onFollow = { session -> followDialogSession = session },
        onUnfollow = { session ->
            viewModel.unfollowSession(session) { result -> toast = result.exceptionOrNull()?.message ?: result.getOrNull() }
        },
        onToggleJob = { job ->
            val toggle = {
                viewModel.toggleJobFollow(job) { result -> toast = result.exceptionOrNull()?.message ?: result.getOrNull() }
            }
            if (job.id in state.followedJobIds) toggle() else withNotificationPermission(toggle)
        },
    )
    // A tapped follow notification: open the report, else expand the agent's card.
    val pendingFollowOpen = li.rajeshgo.sm.push.FollowOpenRequests.pending
    val onFollowOpenConsumed = { li.rajeshgo.sm.push.FollowOpenRequests.pending = null }
    LaunchedEffect(pendingFollowOpen, state.sessions) {
        val open = pendingFollowOpen ?: return@LaunchedEffect
        val readerPath = open.readerPath
        if (readerPath != null) {
            openPage = ReaderPage(title = open.title.ifBlank { "Completion report" }, subtitle = readerPath, path = readerPath)
            onFollowOpenConsumed()
            return@LaunchedEffect
        }
        if (state.sessions.isEmpty() && state.loading) return@LaunchedEffect
        val session = state.sessions.firstOrNull { it.id == open.sessionId }
        if (session != null) {
            viewModel.expandSession(session)
        } else {
            toast = open.title.ifBlank { "That agent is no longer listed" }
        }
        onFollowOpenConsumed()
    }
    val openAttach: (ClientSession) -> Unit = { session ->
        if (session.mobileTerminal?.supported == true) {
            viewModel.openMobileTerminal(session) { result ->
                toast = result.exceptionOrNull()?.message ?: result.getOrNull()
            }
        } else {
            val attach = session.termuxAttach
            if (attach == null) {
                toast = "Attach metadata unavailable"
            } else {
                launchTermuxAttach(context, attach)
                    .onSuccess { toast = "Opening Termux for ${sessionDisplayName(session)}" }
                    .onFailure { error -> toast = error.message ?: "Attach failed" }
            }
        }
    }

    androidx.compose.runtime.DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_STOP) viewModel.setTerminalForeground(false)
            if (event == Lifecycle.Event.ON_START) viewModel.setTerminalForeground(true)
            isResumed = lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
        }
    }

    LaunchedEffect(isResumed, state.serverUrl, state.userEmail) {
        if (!isResumed || state.serverUrl.isBlank()) {
            return@LaunchedEffect
        }
        viewModel.refresh()
        updateViewModel.refresh()
        while (coroutineContext.isActive) {
            delay(WATCH_AUTO_REFRESH_MS)
            viewModel.refresh()
        }
    }

    LaunchedEffect(toast) {
        val currentToast = toast ?: return@LaunchedEffect
        delay(WATCH_TOAST_MS)
        if (toast == currentToast) {
            toast = null
        }
    }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background)
            .statusBarsPadding()
            .navigationBarsPadding(),
    ) {
        if (state.loading) {
            Box(modifier = Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                CircularProgressIndicator(color = Cyan)
            }
            return@Box
        }

        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(start = 16.dp, top = 16.dp, end = 16.dp, bottom = 128.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item {
                HeaderBar(
                    userEmail = state.userEmail,
                    lastSync = state.lastSync,
                    refreshing = state.refreshing,
                    requestingStatus = state.requestingStatus,
                    ensuringMaintainer = state.ensuringMaintainer,
                    hasUpdate = updateState.availableUpdate != null,
                    onRefresh = { viewModel.refresh() },
                    onRequestStatus = {
                        viewModel.requestStatus { result ->
                            toast = result.exceptionOrNull()?.message ?: result.getOrNull()
                        }
                    },
                    onEnsureMaintainer = {
                        viewModel.ensureMaintainer { result ->
                            toast = result.exceptionOrNull()?.message ?: result.getOrNull()
                        }
                    },
                    onOpenSettings = onNavigateToSettings,
                    onNewSession = { cloneSource = null; createError = null; creating = true },
                    onOpenHistory = { openPage = historyReaderPage },
                )
            }

            item {
                SessionFilters(
                    sessions = state.sessions,
                    query = query,
                    filter = filter,
                    onQueryChange = { query = it },
                    onFilterChange = { filter = it },
                )
            }

            updateState.availableUpdate?.let { update ->
                item {
                    UpdateReadyBanner(
                        update = update,
                        onOpenSettings = onNavigateToSettings,
                    )
                }
            }

            if (state.error != null) {
                item {
                    Text(
                        text = state.error.orEmpty(),
                        color = Rose,
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            }

            if (sections.isEmpty()) {
                item {
                    EmptyState(query = query, filter = filter)
                }
            } else {
                activeSections.forEach { section ->
                    item(key = "repo-${section.repoKey}") {
                        RepoHeader(title = "${section.repoLabel.removeSuffix("/")} · Active")
                    }
                    items(section.roots, key = { "active-${it.session.id}" }) { root ->
                        WatchTree(
                            node = root,
                            depth = 0,
                            slice = TreeSlice.Active,
                            sessionsById = sessionsById,
                            expandedSessionIds = state.expandedSessionIds,
                            detailsById = state.detailsBySessionId,
                            whatById = state.whatBySessionId,
                            onToggleExpanded = { viewModel.toggleExpanded(it) },
                            onOpenAttach = openAttach,
                            onClone = { cloneSource = it; createError = null; creating = true },
                            onCopyAttach = { session ->
                                val command = session.termuxAttach?.let(::termuxAttachCommand)
                                if (command == null) {
                                    toast = "Attach command unavailable"
                                } else {
                                    val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                                    clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm attach", command))
                                    toast = "Attach command copied"
                                }
                            },
                            onOpenTelegram = { session ->
                                val link = telegramLink(session)
                                if (link == null) {
                                    toast = "Telegram thread unavailable"
                                } else {
                                    runCatching {
                                        context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(link)).apply {
                                            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                                        })
                                    }.onFailure { error ->
                                        toast = error.message ?: "Unable to open Telegram"
                                    }
                                }
                            },
                            onWhat = viewModel::requestWhat,
                            onUpdateWhat = viewModel::updateWhat,
                            onRegenerateWhat = viewModel::regenerateWhat,
                            onKill = { session ->
                                viewModel.retireSession(session.id) { result ->
                                    toast = result.exceptionOrNull()?.message ?: "Retired ${session.id}"
                                }
                            },
                            onOpenPage = { openPage = it },
                            follow = follow,
                        )
                    }
                }
                idleSections.forEach { section ->
                    item(key = "idle-repo-${section.repoKey}") {
                        RepoHeader(title = "${section.repoLabel.removeSuffix("/")} · Idle")
                    }
                    items(section.roots, key = { "idle-${it.session.id}" }) { root ->
                        WatchTree(
                            node = root,
                            depth = 0,
                            slice = TreeSlice.Idle,
                            sessionsById = sessionsById,
                            expandedSessionIds = state.expandedSessionIds,
                            detailsById = state.detailsBySessionId,
                            whatById = state.whatBySessionId,
                            onToggleExpanded = { viewModel.toggleExpanded(it) },
                            onOpenAttach = openAttach,
                            onClone = { cloneSource = it; createError = null; creating = true },
                            onCopyAttach = { session ->
                                val command = session.termuxAttach?.let(::termuxAttachCommand)
                                if (command == null) {
                                    toast = "Attach command unavailable"
                                } else {
                                    val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                                    clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm attach", command))
                                    toast = "Attach command copied"
                                }
                            },
                            onOpenTelegram = { session ->
                                val link = telegramLink(session)
                                if (link == null) {
                                    toast = "Telegram thread unavailable"
                                } else {
                                    runCatching {
                                        context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(link)).apply {
                                            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                                        })
                                    }.onFailure { error ->
                                        toast = error.message ?: "Unable to open Telegram"
                                    }
                                }
                            },
                            onWhat = viewModel::requestWhat,
                            onUpdateWhat = viewModel::updateWhat,
                            onRegenerateWhat = viewModel::regenerateWhat,
                            onKill = { session ->
                                viewModel.retireSession(session.id) { result ->
                                    toast = result.exceptionOrNull()?.message ?: "Retired ${session.id}"
                                }
                            },
                            onOpenPage = { openPage = it },
                            follow = follow,
                        )
                    }
                }
            }


        }

        Box(
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .padding(horizontal = 16.dp, vertical = 16.dp),
        ) {
            AppBottomNav(
                currentRoute = Routes.WATCH,
                onWatch = {},
                onAnalytics = onNavigateToAnalytics,
            )
        }

        if (toast != null) {
            Box(modifier = Modifier.fillMaxSize(), contentAlignment = Alignment.BottomCenter) {
                Surface(
                    modifier = Modifier.padding(start = 16.dp, end = 16.dp, bottom = 88.dp),
                    shape = RoundedCornerShape(999.dp),
                    color = PanelElevated,
                    border = androidx.compose.foundation.BorderStroke(1.dp, BorderStrong),
                ) {
                    Text(
                        text = toast.orEmpty(),
                        modifier = Modifier.padding(horizontal = 16.dp, vertical = 10.dp),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurface,
                    )
                }
            }
        }

        if (creating) {
            CreateSessionSheet(cloneSource, state.sessions, viewModel::sessionModels, createBusy, createError, onDismiss = { creating = false }) { request ->
                createBusy = true
                createError = null
                viewModel.createSession(request) { result ->
                    createBusy = false
                    result.onSuccess { creating = false; toast = it }
                    result.onFailure { createError = it.message ?: "Could not create session" }
                }
            }
        }
        state.terminal?.let { terminal ->
            MobileTerminalOverlay(
                terminal = terminal,
                onInputChange = viewModel::updateTerminalInput,
                onSend = { viewModel.sendTerminalInput(sendEnter = true) },
                onChangeModel = viewModel::openTerminalModelMenu,
                onTerminalInput = viewModel::sendTerminalData,
                onTerminalResize = viewModel::resizeTerminal,
                onTerminalPageScroll = viewModel::sendTerminalPageScroll,
                onRendererStatus = viewModel::markTerminalRendererStatus,
                onRendererReady = viewModel::markTerminalRendererReady,
                onRendererError = viewModel::markTerminalRendererError,
                onRendererWritten = viewModel::markTerminalRendererWritten,
                onDetach = viewModel::detachTerminal,
                onReconnect = viewModel::reconnectTerminal,
                onCopy = { selectedText ->
                    val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                    val copiedText = selectedText.ifBlank { terminal.copyBuffer }
                    clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm terminal", copiedText))
                    toast = "Terminal output copied"
                },
            )
        }
        followDialogSession?.let { session ->
            FollowDialog(
                session = session,
                onDismiss = { followDialogSession = null },
                onFollow = { message ->
                    followDialogSession = null
                    withNotificationPermission {
                        viewModel.followSession(session, message) { result ->
                            toast = result.exceptionOrNull()?.message ?: result.getOrNull()
                        }
                    }
                },
            )
        }
        openPage?.let { page ->
            DocReaderOverlay(
                page = page,
                loadAuth = viewModel::docReaderAuth,
                onClose = { openPage = null },
                onCopyLink = { link ->
                    val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                    clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm doc", link))
                    toast = "Doc link copied"
                },
            )
        }
    }
}

@Composable
private fun MobileTerminalOverlay(
    terminal: TerminalUiState,
    onInputChange: (String) -> Unit,
    onSend: () -> Unit,
    onChangeModel: () -> Unit,
    onTerminalInput: (String) -> Unit,
    onTerminalResize: (cols: Int, rows: Int) -> Unit,
    onTerminalPageScroll: (up: Boolean) -> Unit,
    onRendererStatus: (String) -> Unit,
    onRendererReady: (cols: Int, rows: Int) -> Unit,
    onRendererError: (String) -> Unit,
    onRendererWritten: (sequence: Long, bytes: Int) -> Unit,
    onDetach: () -> Unit,
    onReconnect: () -> Unit,
    onCopy: (String) -> Unit,
) {
    var copyRequest by remember { mutableStateOf(0L) }
    val controls = remember(terminal.connectionGeneration) { TerminalControls() }
    var actionsOpen by remember { mutableStateOf(false) }
    val keyboard = androidx.compose.ui.platform.LocalSoftwareKeyboardController.current
    val focus = androidx.compose.ui.platform.LocalFocusManager.current
    BackHandler(onBack = onDetach)

    Surface(
        modifier = Modifier.fillMaxSize(),
        color = MaterialTheme.colorScheme.background,
    ) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .imePadding()
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Surface(
                shape = RoundedCornerShape(12.dp),
                color = Panel,
                border = androidx.compose.foundation.BorderStroke(1.dp, BorderStrong),
            ) {
                Row(
                    modifier = Modifier.fillMaxWidth().padding(horizontal = 10.dp, vertical = 6.dp),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(modifier = Modifier.weight(1f)) {
                        Text(
                            text = terminal.title,
                            style = MaterialTheme.typography.titleMedium,
                            color = MaterialTheme.colorScheme.onSurface,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                        Text(
                            text = terminalConnectionLabel(terminal.status),
                            style = MaterialTheme.typography.labelSmall,
                            color = if (terminal.error == null) Cyan else Rose,
                            fontFamily = FontFamily.Monospace,
                        )

                    }
                    OutlinedButton(
                        onClick = onDetach,
                        modifier = Modifier.height(44.dp),
                        contentPadding = PaddingValues(horizontal = 10.dp, vertical = 2.dp),
                    ) {
                        Text("Close")
                    }
                }
            }

            if (terminal.status in setOf("failed", "detached")) {
                TextButton(onClick = onReconnect) { Text("Reconnect") }
            }
            terminal.error?.let { error ->
                Text(
                    text = error,
                    color = Rose,
                    style = MaterialTheme.typography.bodySmall,
                )
            }

            Surface(
                modifier = Modifier.weight(1f).fillMaxWidth(),
                shape = RoundedCornerShape(12.dp),
                color = Color.Black,
                border = androidx.compose.foundation.BorderStroke(1.dp, BorderStrong),
            ) {
                key(terminal.connectionGeneration) {
                TerminalWebView(
                    terminal = terminal,
                    copyRequest = copyRequest,
                    controls = controls,
                    onInput = onTerminalInput,
                    onResize = onTerminalResize,
                    onPageScroll = onTerminalPageScroll,
                    onRendererStatus = onRendererStatus,
                    onRendererReady = onRendererReady,
                    onRendererError = onRendererError,
                    onRendererWritten = onRendererWritten,
                    onCopyText = onCopy,
                )
                }
            }

            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                val connected = terminal.status == "attached"
                listOf("up" to "↑", "down" to "↓", "left" to "←", "right" to "→", "enter" to "↵").forEach { (key, label) ->
                    OutlinedButton(
                        onClick = { controls.sendKey(key, onTerminalInput) },
                        enabled = connected && controls.canSend(key),
                        modifier = Modifier.weight(1f).height(48.dp).semantics { contentDescription = if (key == "enter") "Enter" else "${key.replaceFirstChar(Char::uppercaseChar)} arrow" },
                        contentPadding = PaddingValues(0.dp),
                    ) { Text(label, style = MaterialTheme.typography.titleLarge) }
                }
                Box(Modifier.weight(1f)) {
                    OutlinedButton(onClick = { actionsOpen = true }, modifier = Modifier.fillMaxWidth().height(48.dp), contentPadding = PaddingValues(0.dp)) {
                        Icon(Icons.Rounded.MoreHoriz, "Terminal actions")
                    }
                    DropdownMenu(expanded = actionsOpen, onDismissRequest = { actionsOpen = false }) {
                        if (supportsSessionCloning(terminal.provider)) {
                            DropdownMenuItem(text = { Text("Change model") }, enabled = connected, onClick = {
                                actionsOpen = false; focus.clearFocus(); keyboard?.hide(); onChangeModel()
                            })
                            HorizontalDivider()
                        }
                        listOf("esc" to "Escape", "tab" to "Tab", "shift-tab" to "Shift + Tab", "backspace" to "Backspace", "ctrl-c" to "Interrupt (Ctrl-C)").forEach { (key, label) ->
                            DropdownMenuItem(text = { Text(label) }, enabled = connected, onClick = { actionsOpen = false; controls.sendKey(key, onTerminalInput) })
                        }
                        HorizontalDivider()
                        DropdownMenuItem(text = { Text("Copy terminal") }, onClick = { actionsOpen = false; copyRequest += 1 })
                    }
                }
            }

            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(6.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                OutlinedTextField(
                    value = terminal.inputDraft,
                    onValueChange = onInputChange,
                    modifier = Modifier.weight(1f),
                    label = { Text("Message agent") },
                    keyboardOptions = KeyboardOptions(capitalization = KeyboardCapitalization.Sentences, keyboardType = KeyboardType.Text),
                    singleLine = false,
                    maxLines = 6,
                )
                Button(
                    onClick = {
                        onSend()
                        focus.clearFocus(force = true)
                        keyboard?.hide()
                    },
                    enabled = terminal.status == "attached" && terminal.inputDraft.isNotBlank(),
                    modifier = Modifier.height(42.dp),
                    contentPadding = ButtonDefaults.ContentPadding,
                ) {
                    Text("Send")
                }
            }
        }
    }
}

@SuppressLint("SetJavaScriptEnabled")
@Suppress("DEPRECATION")
@Composable
private fun TerminalWebView(
    terminal: TerminalUiState,
    copyRequest: Long,
    controls: TerminalControls,
    onInput: (String) -> Unit,
    onResize: (cols: Int, rows: Int) -> Unit,
    onPageScroll: (up: Boolean) -> Unit,
    onRendererStatus: (String) -> Unit,
    onRendererReady: (cols: Int, rows: Int) -> Unit,
    onRendererError: (String) -> Unit,
    onRendererWritten: (sequence: Long, bytes: Int) -> Unit,
    onCopyText: (String) -> Unit,
) {
    var deliveredSequence by remember { mutableStateOf(0L) }
    var deliveredCopyRequest by remember { mutableStateOf(0L) }
    var terminalReady by remember { mutableStateOf(false) }
    var webViewRef by remember { mutableStateOf<WebView?>(null) }
    var claudePageScrollRemainder by remember { mutableStateOf(0f) }
    var draggingScrollbar by remember { mutableStateOf(false) }
    val historyScrollbarVisible = remember { mutableStateOf(false) }
    val density = LocalDensity.current.density
    val claudeDirectPageScroll = terminal.provider == "claude"

    androidx.compose.runtime.DisposableEffect(Unit) {
        onDispose {
            controls.rendererReady = false
            controls.sendArrow = null
            webViewRef?.destroy()
            webViewRef = null
        }
    }

    AndroidView(
        modifier = Modifier
            .fillMaxSize()
            .pointerInput(density, claudeDirectPageScroll) {
                detectVerticalDragGestures(
                    onDragStart = { position ->
                        claudePageScrollRemainder = 0f
                        // Match the 28 CSS-pixel history rail in terminal.html.
                        draggingScrollbar = historyScrollbarVisible.value &&
                            position.x >= size.width - 28f * density
                        if (draggingScrollbar) webViewRef?.evaluateJavascript(
                            "window.smBeginScrollbarDrag(${position.y / density}, true);", null,
                        )
                    },
                    onDragEnd = {
                        claudePageScrollRemainder = 0f
                        draggingScrollbar = false
                        webViewRef?.evaluateJavascript("window.smEndScrollbarDrag();", null)
                    },
                    onDragCancel = {
                        claudePageScrollRemainder = 0f
                        draggingScrollbar = false
                        webViewRef?.evaluateJavascript("window.smEndScrollbarDrag();", null)
                    },
                ) { change, dragAmount ->
                        change.consume()
                        if (draggingScrollbar && !historyScrollbarVisible.value) {
                            draggingScrollbar = false
                            webViewRef?.evaluateJavascript("window.smEndScrollbarDrag();", null)
                        }
                        if (draggingScrollbar) {
                            webViewRef?.evaluateJavascript(
                                "window.smDragScrollbar(${change.position.y / density});", null,
                            )
                            return@detectVerticalDragGestures
                        }
                        val cssDelta = -dragAmount / density
                        if (claudeDirectPageScroll) {
                            claudePageScrollRemainder += cssDelta
                            val threshold = 72f
                            if (claudePageScrollRemainder <= -threshold) {
                                onPageScroll(true)
                                claudePageScrollRemainder = 0f
                            } else if (claudePageScrollRemainder >= threshold) {
                                onPageScroll(false)
                                claudePageScrollRemainder = 0f
                            }
                            return@detectVerticalDragGestures
                        }
                        val cssX = change.position.x / density
                        val cssY = change.position.y / density
                        webViewRef?.evaluateJavascript(
                            "window.smScrollPixels($cssDelta, $cssX, $cssY);",
                            null,
                        )
                }
            },
        factory = { context ->
            WebView(context).apply {
                controls.sendArrow = { key -> evaluateJavascript("window.smSendKey(${jsString(key)});", null) }
                setBackgroundColor(android.graphics.Color.rgb(5, 8, 13))
                settings.javaScriptEnabled = true
                settings.domStorageEnabled = false
                settings.allowContentAccess = false
                settings.allowFileAccess = false
                settings.allowFileAccessFromFileURLs = false
                settings.allowUniversalAccessFromFileURLs = false
                settings.cacheMode = WebSettings.LOAD_NO_CACHE
                settings.blockNetworkLoads = false
                webViewClient = object : WebViewClient() {
                    override fun shouldOverrideUrlLoading(view: WebView?, request: WebResourceRequest?): Boolean {
                        val url = request?.url ?: return true
                        return url.scheme != "https" || url.host != TERMINAL_ASSET_HOST
                    }

                    override fun shouldInterceptRequest(
                        view: WebView?,
                        request: WebResourceRequest?,
                    ): WebResourceResponse {
                        return terminalAssetResponse(context, request?.url)
                    }
                }
                addJavascriptInterface(
                    TerminalJavascriptBridge(
                        onInput = onInput,
                        onResize = onResize,
                        onCopyText = onCopyText,
                        onStatus = onRendererStatus,
                        onReady = { cols, rows ->
                            terminalReady = true
                            controls.rendererReady = true
                            onRendererReady(cols, rows)
                        },
                        onError = { message ->
                            controls.rendererReady = false
                            onRendererError(message)
                        },
                        onWritten = onRendererWritten,
                        onScrollbarVisibility = { historyScrollbarVisible.value = it },
                    ),
                    "TerminalBridge",
                )
                loadUrl(TERMINAL_ASSET_URL)
                webViewRef = this
            }
        },
        update = { webView ->
            // Configure provider-owned history before delivering its output.
            if (terminalReady) webView.evaluateJavascript("window.smSetProvider(${jsString(terminal.provider.orEmpty())});", null)
            webView.evaluateJavascript("window.smSetStatus(${jsString(terminal.status)});", null)
            if (terminalReady) {
                terminal.outputFrames
                    .filter { it.sequence > deliveredSequence }
                    .forEach { frame ->
                        if (frame.encoding == "base64") {
                            webView.evaluateJavascript("window.smWriteBase64(${frame.sequence}, ${jsString(frame.data)});", null)
                        } else {
                            webView.evaluateJavascript("window.smWriteText(${frame.sequence}, ${jsString(frame.data)});", null)
                        }
                        deliveredSequence = frame.sequence
                    }
            }
            if (copyRequest != deliveredCopyRequest) {
                deliveredCopyRequest = copyRequest
                webView.evaluateJavascript("window.smCopySelection();", null)
            }
        },
    )
}

private fun terminalDiagnostics(terminal: TerminalUiState): String {
    val frameSummary = "${terminal.outputFrameCount} frames/${formatDiagnosticBytes(terminal.outputByteCount)}"
    val ackSummary = if (terminal.rendererLastAckSequence > 0) {
        "ack ${terminal.rendererLastAckSequence}/${terminal.outputSequence}"
    } else {
        "ack -/${terminal.outputSequence}"
    }
    val rendererMessage = terminal.rendererError ?: terminal.rendererStatus
    return "$rendererMessage • $frameSummary • $ackSummary"
}

private fun formatDiagnosticBytes(bytes: Long): String {
    if (bytes < 1024) {
        return "${bytes}B"
    }
    if (bytes < 1024 * 1024) {
        return "${bytes / 1024}KiB"
    }
    return "${bytes / (1024 * 1024)}MiB"
}

private const val TERMINAL_ASSET_HOST = "sm-terminal.local"
private const val TERMINAL_ASSET_URL = "https://$TERMINAL_ASSET_HOST/terminal.html"

private fun terminalAssetResponse(context: android.content.Context, uri: Uri?): WebResourceResponse {
    if (uri?.scheme != "https" || uri.host != TERMINAL_ASSET_HOST) {
        return blockedTerminalAssetResponse()
    }
    val assetPath = when (uri.path) {
        "/terminal.html" -> "sm_terminal/terminal.html"
        "/terminal_keys.js" -> "sm_terminal/terminal_keys.js"
        "/vendor/xterm.css" -> "sm_terminal/vendor/xterm.css"
        "/vendor/xterm.js" -> "sm_terminal/vendor/xterm.js"
        "/vendor/addon-fit.js" -> "sm_terminal/vendor/addon-fit.js"
        else -> return blockedTerminalAssetResponse()
    }
    val mimeType = when {
        assetPath.endsWith(".html") -> "text/html"
        assetPath.endsWith(".css") -> "text/css"
        assetPath.endsWith(".js") -> "application/javascript"
        else -> "application/octet-stream"
    }
    return try {
        WebResourceResponse(mimeType, "UTF-8", context.assets.open(assetPath)).apply {
            setResponseHeaders(mapOf("Cache-Control" to "no-store"))
        }
    } catch (_: Exception) {
        WebResourceResponse(
            "text/plain",
            "UTF-8",
            404,
            "Not Found",
            mapOf("Cache-Control" to "no-store"),
            ByteArrayInputStream(ByteArray(0)),
        )
    }
}

private fun blockedTerminalAssetResponse(): WebResourceResponse {
    return WebResourceResponse(
        "text/plain",
        "UTF-8",
        403,
        "Forbidden",
        mapOf("Cache-Control" to "no-store"),
        ByteArrayInputStream(ByteArray(0)),
    )
}

private class TerminalJavascriptBridge(
    private val onInput: (String) -> Unit,
    private val onResize: (cols: Int, rows: Int) -> Unit,
    private val onCopyText: (String) -> Unit,
    private val onStatus: (String) -> Unit,
    private val onReady: (cols: Int, rows: Int) -> Unit,
    private val onError: (String) -> Unit,
    private val onWritten: (sequence: Long, bytes: Int) -> Unit,
    private val onScrollbarVisibility: (Boolean) -> Unit,
) {
    private val mainHandler = Handler(Looper.getMainLooper())

    @JavascriptInterface
    fun scrollbarVisibility(visible: Boolean) {
        mainHandler.post { onScrollbarVisibility(visible) }
    }

    @JavascriptInterface
    fun input(data: String) {
        mainHandler.post { onInput(data) }
    }

    @JavascriptInterface
    fun resize(cols: Int, rows: Int) {
        mainHandler.post { onResize(cols, rows) }
    }

    @JavascriptInterface
    fun copy(text: String) {
        mainHandler.post { onCopyText(text) }
    }

    @JavascriptInterface
    fun status(message: String) {
        mainHandler.post { onStatus(message) }
    }

    @JavascriptInterface
    fun ready(cols: Int, rows: Int) {
        mainHandler.post { onReady(cols, rows) }
    }

    @JavascriptInterface
    fun error(message: String) {
        mainHandler.post { onError(message) }
    }

    @JavascriptInterface
    fun written(sequence: String, bytes: Int) {
        val parsedSequence = sequence.toLongOrNull() ?: 0L
        mainHandler.post { onWritten(parsedSequence, bytes) }
    }
}

private fun jsString(value: String): String = JSONObject.quote(value)

@Composable
private fun StudioSshToggleCard(
    enabled: Boolean,
    status: String,
    host: String,
    busy: Boolean,
    error: String?,
    onToggle: (Boolean) -> Unit,
) {
    val displayHost = host.ifBlank { "studio-ssh.rajeshgo.li" }
    Surface(
        shape = RoundedCornerShape(18.dp),
        color = Panel,
        border = androidx.compose.foundation.BorderStroke(1.dp, Border),
    ) {
        Column(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 12.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(modifier = Modifier.weight(1f), verticalAlignment = Alignment.CenterVertically) {
                    Box(modifier = Modifier.size(9.dp).background(studioSshStatusTint(status), CircleShape))
                    Spacer(Modifier.width(10.dp))
                    Column(modifier = Modifier.weight(1f)) {
                        Text(
                            text = "Studio SSH",
                            style = MaterialTheme.typography.titleMedium,
                            color = MaterialTheme.colorScheme.onSurface,
                        )
                        Spacer(Modifier.height(2.dp))
                        Text(
                            text = displayHost,
                            style = MaterialTheme.typography.labelSmall,
                            color = TextMuted,
                            fontFamily = FontFamily.Monospace,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                Spacer(Modifier.width(10.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    StatusChip(label = status, tint = studioSshStatusTint(status))
                    Spacer(Modifier.width(10.dp))
                    Switch(
                        checked = enabled,
                        onCheckedChange = { desired -> if (!busy) onToggle(desired) },
                        enabled = !busy,
                    )
                }
            }
            Text(
                text = "ssh studio-away",
                style = MaterialTheme.typography.labelSmall,
                color = Cyan,
                fontFamily = FontFamily.Monospace,
            )
            error?.let { message ->
                Text(
                    text = message,
                    style = MaterialTheme.typography.bodySmall,
                    color = Rose,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

private fun studioSshStatusTint(status: String): Color = when (status.lowercase()) {
    "on" -> Emerald
    "starting" -> Amber
    "error" -> Rose
    else -> TextMuted
}

@Composable
private fun HeaderBar(
    userEmail: String,
    lastSync: String?,
    refreshing: Boolean,
    requestingStatus: Boolean,
    ensuringMaintainer: Boolean,
    hasUpdate: Boolean,
    onRefresh: () -> Unit,
    onRequestStatus: () -> Unit,
    onEnsureMaintainer: () -> Unit,
    onOpenSettings: () -> Unit,
    onNewSession: () -> Unit,
    onOpenHistory: () -> Unit,
) {
    var menuExpanded by remember { mutableStateOf(false) }

    Surface(
        shape = RoundedCornerShape(18.dp),
        color = Panel,
        border = androidx.compose.foundation.BorderStroke(1.dp, Border),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 10.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(Modifier.weight(1f)) {
                Text("sm watch", style = MaterialTheme.typography.titleLarge, color = MaterialTheme.colorScheme.onSurface)
                val statusLine = buildString {
                    append("Last sync ")
                    append(formatDateTime(lastSync))
                    if (userEmail.isNotBlank()) {
                        append(" • ")
                        append(userEmail)
                    }
                }
                Spacer(Modifier.height(2.dp))
                Text(
                    text = statusLine,
                    style = MaterialTheme.typography.labelSmall,
                    color = if (refreshing) Cyan else TextMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                IconButton(onClick = onOpenHistory) {
                    Icon(Icons.Rounded.History, contentDescription = "History", tint = TextSecondary)
                }
                Box {
                    IconButton(onClick = { menuExpanded = true }) {
                        Icon(
                            Icons.Rounded.MoreVert,
                            contentDescription = "Watch actions",
                            tint = if (refreshing || requestingStatus || ensuringMaintainer) Cyan else TextSecondary,
                        )
                    }
                    DropdownMenu(
                        expanded = menuExpanded,
                        onDismissRequest = { menuExpanded = false },
                    ) {
                        DropdownMenuItem(
                            text = { Text("New session") },
                            leadingIcon = { Icon(Icons.Rounded.Add, contentDescription = null) },
                            onClick = { menuExpanded = false; onNewSession() },
                            enabled = userEmail.isNotBlank(),
                        )
                        DropdownMenuItem(
                            text = { Text(if (ensuringMaintainer) "Wake maintainer (starting...)" else "Wake maintainer") },
                            onClick = {
                                menuExpanded = false
                                onEnsureMaintainer()
                            },
                            enabled = !ensuringMaintainer,
                            leadingIcon = {
                                Icon(
                                    Icons.Rounded.SupportAgent,
                                    contentDescription = null,
                                    tint = if (ensuringMaintainer) Cyan else TextSecondary,
                                )
                            },
                        )
                        DropdownMenuItem(
                            text = { Text(if (requestingStatus) "Request status (sending...)" else "Request status") },
                            onClick = {
                                menuExpanded = false
                                onRequestStatus()
                            },
                            enabled = !requestingStatus,
                            leadingIcon = {
                                Icon(
                                    Icons.Rounded.Campaign,
                                    contentDescription = null,
                                    tint = if (requestingStatus) Cyan else TextSecondary,
                                )
                            },
                        )
                        DropdownMenuItem(
                            text = { Text(if (refreshing) "Refresh (running...)" else "Refresh") },
                            onClick = {
                                menuExpanded = false
                                onRefresh()
                            },
                            enabled = !refreshing,
                            leadingIcon = {
                                Icon(
                                    Icons.Rounded.Refresh,
                                    contentDescription = null,
                                    tint = if (refreshing) Cyan else TextSecondary,
                                )
                            },
                        )

                    }
                }
                SettingsIconButtonWithUpdate(hasUpdate = hasUpdate, onClick = onOpenSettings)
            }
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun SessionFilters(
    sessions: List<ClientSession>,
    query: String,
    filter: String,
    onQueryChange: (String) -> Unit,
    onFilterChange: (String) -> Unit,
) {
    var filtersOpen by remember { mutableStateOf(false) }
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        OutlinedTextField(
            value = query, onValueChange = onQueryChange, modifier = Modifier.weight(1f),
            placeholder = { Text("Search agents") }, singleLine = true, shape = RoundedCornerShape(14.dp),
        )
        Box {
            TextButton(onClick = { filtersOpen = true }) {
                Text(if (filter == "all") "All agents" else filter.replaceFirstChar { it.titlecase() })
            }
            DropdownMenu(filtersOpen, { filtersOpen = false }) {
                listOf("all", "running", "idle", "stopped").forEach { candidate ->
                    DropdownMenuItem(text = { Text(if (candidate == "all") "All agents" else candidate.replaceFirstChar { it.titlecase() }) }, onClick = { onFilterChange(candidate); filtersOpen = false })
                }
            }
        }
    }
}

@Composable
private fun SummaryBadge(label: String, tint: Color) {
    Surface(
        shape = RoundedCornerShape(999.dp),
        color = tint.copy(alpha = 0.12f),
        border = androidx.compose.foundation.BorderStroke(1.dp, tint.copy(alpha = 0.24f)),
    ) {
        Text(
            text = label,
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp),
            style = MaterialTheme.typography.labelSmall,
            color = tint,
            fontWeight = FontWeight.Medium,
        )
    }
}

@Composable
private fun RepoHeader(title: String) {
    Text(
        text = title,
        style = MaterialTheme.typography.labelSmall,
        color = Cyan,
        fontFamily = FontFamily.Monospace,
    )
}

@Composable
private fun WatchTree(
    node: WatchSessionNode,
    depth: Int,
    slice: TreeSlice,
    sessionsById: Map<String, ClientSession>,
    expandedSessionIds: Set<String>,
    detailsById: Map<String, SessionDetail>,
    whatById: Map<String, WhatUiState>,
    onToggleExpanded: (ClientSession) -> Unit,
    onOpenAttach: (ClientSession) -> Unit,
    onClone: (ClientSession) -> Unit,
    onCopyAttach: (ClientSession) -> Unit,
    onOpenTelegram: (ClientSession) -> Unit,
    onWhat: (ClientSession) -> Unit,
    onUpdateWhat: (ClientSession) -> Unit,
    onRegenerateWhat: (ClientSession) -> Unit,
    onKill: (ClientSession) -> Unit,
    onOpenPage: (ReaderPage) -> Unit,
    follow: FollowUi,
) {
    if (!nodeMatchesSlice(node, slice)) {
        return
    }

    val renderNode = shouldRenderNode(node, slice)
    if (renderNode) {
        SessionRow(
            session = node.session,
            depth = depth,
            parentLabel = parentLabel(node.session, sessionsById),
            expanded = expandedSessionIds.contains(node.session.id),
            detail = detailsById[node.session.id],
            whatState = whatById[node.session.id],
            onToggleExpanded = { onToggleExpanded(node.session) },
            onOpenAttach = { onOpenAttach(node.session) },
            onClone = { onClone(node.session) },
            onCopyAttach = { onCopyAttach(node.session) },
            onOpenTelegram = { onOpenTelegram(node.session) },
            onWhat = { onWhat(node.session) },
            onUpdateWhat = { onUpdateWhat(node.session) },
            onRegenerateWhat = { onRegenerateWhat(node.session) },
            onKill = { onKill(node.session) },
            onOpenPage = onOpenPage,
            follow = follow,
        )
    }

    val childDepth = if (renderNode) depth + 1 else depth

    node.sameRepoChildren
        .filter { nodeMatchesSlice(it, slice) }
        .forEach { child ->
            WatchTree(child, childDepth, slice, sessionsById, expandedSessionIds, detailsById, whatById, onToggleExpanded, onOpenAttach, onClone, onCopyAttach, onOpenTelegram, onWhat, onUpdateWhat, onRegenerateWhat, onKill, onOpenPage, follow)
    }

    node.crossRepoGroups.forEach { group ->
        val visibleChildren = group.children.filter { nodeMatchesSlice(it, slice) }
        if (visibleChildren.isEmpty()) {
            return@forEach
        }
        val groupDepth = if (renderNode) depth + 1 else depth
        Text(
            text = "${group.repoLabel} (${group.repoKey})",
            modifier = Modifier.padding(start = (groupDepth * 18).dp, top = 2.dp, bottom = 6.dp),
            style = MaterialTheme.typography.labelSmall,
            color = TextMuted,
            fontFamily = FontFamily.Monospace,
        )
        visibleChildren.forEach { child ->
            WatchTree(child, groupDepth + 1, slice, sessionsById, expandedSessionIds, detailsById, whatById, onToggleExpanded, onOpenAttach, onClone, onCopyAttach, onOpenTelegram, onWhat, onUpdateWhat, onRegenerateWhat, onKill, onOpenPage, follow)
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun SessionRow(
    session: ClientSession,
    depth: Int,
    parentLabel: String,
    expanded: Boolean,
    detail: SessionDetail?,
    whatState: WhatUiState?,
    onToggleExpanded: () -> Unit,
    onOpenAttach: () -> Unit,
    onClone: () -> Unit,
    onCopyAttach: () -> Unit,
    onOpenTelegram: () -> Unit,
    onWhat: () -> Unit,
    onUpdateWhat: () -> Unit,
    onRegenerateWhat: () -> Unit,
    onKill: () -> Unit,
    onOpenPage: (ReaderPage) -> Unit,
    follow: FollowUi,
) {
    val followed = session.id in follow.followedSessionIds
    val attachSupported = session.mobileTerminal?.supported == true || session.termuxAttach?.supported == true
    val hasSummary = whatState?.entries?.isNotEmpty() == true
    Surface(
        modifier = Modifier.padding(start = (depth * 14).dp),
        shape = RoundedCornerShape(22.dp),
        color = Panel,
        border = androidx.compose.foundation.BorderStroke(1.dp, Border),
    ) {
        Column(modifier = Modifier.fillMaxWidth()) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .clickable { if (attachSupported) onOpenAttach() else onToggleExpanded() }
                    .padding(14.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(modifier = Modifier.weight(1f), verticalAlignment = Alignment.CenterVertically) {
                    Box(modifier = Modifier.size(9.dp).background(statusDot(session), CircleShape))
                    Spacer(Modifier.width(10.dp))
                    Column(modifier = Modifier.weight(1f)) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(
                                text = sessionDisplayName(session),
                                style = MaterialTheme.typography.titleMedium,
                                color = MaterialTheme.colorScheme.onSurface,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                            if (session.isMaintainer) {
                                Spacer(Modifier.width(8.dp))
                                InlineBadge("maintainer", Cyan)
                            }
                            if (followed) {
                                Spacer(Modifier.width(6.dp))
                                Icon(Icons.Rounded.NotificationsActive, contentDescription = "Following", tint = Amber, modifier = Modifier.size(16.dp))
                            }
                        }
                        Spacer(Modifier.height(3.dp))
                        Text(
                            text = "${session.provider ?: "claude"} · ${projectedStatusLabel(session)}",
                            style = MaterialTheme.typography.bodySmall,
                            color = TextSecondary,
                            fontFamily = FontFamily.Monospace,
                        )
                        Spacer(Modifier.height(6.dp))
                        waitingSummary(session)?.let { waiting ->
                            Text(waiting, style = MaterialTheme.typography.bodySmall, color = Cyan, maxLines = 3, overflow = TextOverflow.Ellipsis)
                            Spacer(Modifier.height(6.dp))
                        }
                        statusSummary(session)?.let { status ->
                            Text(
                                text = status,
                                style = MaterialTheme.typography.bodySmall,
                                color = Cyan,
                                maxLines = 2,
                                overflow = TextOverflow.Ellipsis,
                            )
                            Spacer(Modifier.height(6.dp))
                        }
                        val secondaryLine = buildString {
                            if (parentLabel != "-") {
                                append("Parent ")
                                append(parentLabel)
                                append(" • ")
                            }
                            append(lastSummary(session))
                            append(" • ")
                            append(formatAge(session.lastActivity, session.activityState))
                        }
                        Text(
                            text = secondaryLine,
                            style = MaterialTheme.typography.bodySmall,
                            color = TextMuted,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                Spacer(Modifier.width(10.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    if (attachSupported) {
                        IconButton(onClick = onOpenAttach) {
                            Icon(Icons.Rounded.Terminal, contentDescription = "Attach", tint = Emerald)
                        }
                    }
                    IconButton(onClick = onToggleExpanded) {
                        Icon(
                            if (expanded) Icons.Rounded.UnfoldLess else Icons.Rounded.UnfoldMore,
                            contentDescription = if (expanded) "Collapse" else "Expand",
                            tint = TextSecondary,
                        )
                    }
                }
            }

            if (expanded) {
                HorizontalDivider(color = Border)
                Column(modifier = Modifier.fillMaxWidth().padding(14.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                    FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        StatusChip(label = projectedStatusLabel(session), tint = statusTint(session))
                        StatusChip(label = session.provider ?: "claude", tint = providerTint(session.provider))
                        if (session.role != null) StatusChip(label = session.role, tint = Violet)
                        formatContextPercentage(detail?.contextPercentage)?.let { percentage ->
                            StatusChip(
                                label = "context $percentage",
                                tint = when (detail?.contextState) {
                                    "critical" -> Rose
                                    "warning" -> Amber
                                    else -> TextSecondary
                                },
                            )
                        }
                    }
                    var actionsExpanded by remember { mutableStateOf(false) }
                    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        if (attachSupported) ActionPill(label = "Open terminal", icon = Icons.Rounded.Terminal, onClick = onOpenAttach, tint = Emerald)
                        if (supportsSessionCloning(session.provider)) ActionPill(label = "Clone", icon = Icons.Rounded.ContentCopy, onClick = onClone)
                        Box {
                            IconButton(onClick = { actionsExpanded = true }) { Icon(Icons.Rounded.MoreVert, "Agent actions", tint = TextSecondary) }
                            DropdownMenu(actionsExpanded, { actionsExpanded = false }) {
                                if (attachSupported) DropdownMenuItem(text = { Text("Copy attach command") }, onClick = { actionsExpanded = false; onCopyAttach() })
                                if (telegramLink(session) != null) DropdownMenuItem(text = { Text("Open in Telegram") }, onClick = { actionsExpanded = false; onOpenTelegram() })
                                if (followed) {
                                    DropdownMenuItem(text = { Text("Unfollow") }, onClick = { actionsExpanded = false; follow.onUnfollow(session) })
                                } else {
                                    DropdownMenuItem(text = { Text("Follow") }, enabled = !isStoppedSession(session), onClick = { actionsExpanded = false; follow.onFollow(session) })
                                }
                                DropdownMenuItem(text = { Text("Retire session", color = Rose) }, onClick = { actionsExpanded = false; onKill() })
                            }
                        }
                    }
                    AgentWorkSections(session, onOpenPage, follow)
                    if (hasSummary || whatState?.status?.let { it != "idle" } == true) {
                        AgentDisclosure("Summary", relativeSummaryAge(whatState?.entries?.lastOrNull()?.createdAt)) {
                            whatState?.let { WhatSummarySection(it, onUpdateWhat, onRegenerateWhat) }
                        }
                    } else {
                        TextButton(onClick = onWhat) { Text("Summarize progress") }
                    }
                    val activity = detail?.actionLines.orEmpty().filterNot { it == "-" || it.startsWith("n/a") }
                    if (activity.isNotEmpty()) AgentDisclosure("Recent activity", "${activity.size} recent actions") {
                        activity.forEach { ActivityDetail(it, null, TextSecondary) }
                    }
                    AgentDisclosure("Session details", listOfNotNull(session.model, session.reasoningEffort?.let { "$it effort" }).joinToString(" · ").ifBlank { session.provider ?: "Claude" }) {
                        ActivityDetail("Workspace", session.workingDir, TextSecondary)
                        ActivityDetail("Session ID", session.id, TextMuted)
                        session.pendingAdoptionProposals.filter { (it.status ?: "pending") == "pending" }.forEach {
                            ActivityDetail("Adoption request", "From ${it.proposerName ?: it.proposerSessionId ?: "another agent"}", Violet)
                        }
                        detail?.lastError?.let { ActivityDetail("Attention needed", it, Rose) }
                    }
                    AgentDisclosure("Terminal preview", "View recent output") {
                        Text(detail?.tailLines?.joinToString("\n") ?: "Loading output…", style = MaterialTheme.typography.bodySmall, color = TextSecondary, fontFamily = FontFamily.Monospace)
                    }
                    if (session.mobileTerminal?.supported == false && session.termuxAttach?.supported != true) {
                        StatusChip(label = session.mobileTerminal.reason ?: "mobile attach unavailable", tint = TextMuted)
                    } else if (session.termuxAttach?.supported == false && session.mobileTerminal?.supported != true) {
                        StatusChip(label = session.termuxAttach.reason ?: "attach unavailable", tint = TextMuted)
                    }
                }
            }
        }
    }
}

@Composable
private fun AgentDisclosure(title: String, subtitle: String, content: @Composable () -> Unit) {
    var open by remember { mutableStateOf(false) }
    Surface(color = Panel, shape = RoundedCornerShape(12.dp)) {
        Column {
            Row(Modifier.fillMaxWidth().clickable { open = !open }.padding(12.dp), verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text(title, style = MaterialTheme.typography.titleSmall, color = TextSecondary)
                    Text(subtitle, style = MaterialTheme.typography.bodySmall, color = TextMuted, maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
                Icon(if (open) Icons.Rounded.UnfoldLess else Icons.Rounded.UnfoldMore, if (open) "Hide $title" else "Show $title", tint = TextMuted)
            }
            if (open) Column(Modifier.fillMaxWidth().padding(horizontal = 12.dp).padding(bottom = 12.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) { content() }
        }
    }
}

/** Follow state and actions for agents and queue jobs (sm#1569). */
private class FollowUi(
    val followedSessionIds: Set<String>,
    val followedJobIds: Set<String>,
    val onFollow: (ClientSession) -> Unit,
    val onUnfollow: (ClientSession) -> Unit,
    val onToggleJob: (SessionJob) -> Unit,
)

/** A queued or running job with a bell: outline when not followed, filled when followed. */
@Composable
private fun FollowableJobDetail(job: SessionJob, detail: String?, follow: FollowUi) {
    val followed = job.id in follow.followedJobIds
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.weight(1f)) {
            ActivityDetail(job.label, detail, if (job.state == "running") Emerald else Amber)
        }
        IconButton(onClick = { follow.onToggleJob(job) }) {
            Icon(
                if (followed) Icons.Rounded.Notifications else Icons.Rounded.NotificationsNone,
                contentDescription = if (followed) "Unfollow ${job.label}" else "Follow ${job.label}",
                tint = if (followed) Amber else TextMuted,
            )
        }
    }
}

@Composable
private fun FollowDialog(session: ClientSession, onDismiss: () -> Unit, onFollow: (String) -> Unit) {
    var message by remember(session.id) { mutableStateOf(DEFAULT_FOLLOW_MESSAGE) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Follow ${sessionDisplayName(session)}") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    value = message,
                    onValueChange = { message = it },
                    modifier = Modifier.fillMaxWidth(),
                    minLines = 5,
                    maxLines = 10,
                    label = { Text("Message to the agent") },
                )
                Text(
                    "Clear the message to follow without telling the agent",
                    style = MaterialTheme.typography.bodySmall,
                    color = TextMuted,
                )
            }
        },
        confirmButton = { TextButton(onClick = { onFollow(message) }) { Text("Follow") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun ActivityDetail(title: String, detail: String?, tint: Color) {
    Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
        Box(Modifier.padding(top = 6.dp).size(6.dp).background(tint, CircleShape))
        Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(title, style = MaterialTheme.typography.bodyMedium, color = tint)
            detail?.takeIf { it.isNotBlank() }?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = TextMuted) }
        }
    }
}

@Composable
private fun AgentWorkSections(session: ClientSession, onOpenPage: (ReaderPage) -> Unit, follow: FollowUi) {
    val claims = workClaims(session.obligations?.claims.orEmpty())
    if (claims.isNotEmpty()) WorkLine(claims, onOpenPage)
    val docs = session.obligations?.docs.orEmpty()
    if (docs.isNotEmpty()) {
        Surface(color = Emerald.copy(alpha = 0.06f), shape = RoundedCornerShape(12.dp)) {
            Column(Modifier.fillMaxWidth().padding(vertical = 8.dp)) {
                Text("Docs", style = MaterialTheme.typography.titleSmall, color = Emerald, modifier = Modifier.padding(horizontal = 14.dp, vertical = 6.dp))
                docs.forEach { doc -> DocRow(doc, onClick = { onOpenPage(docReaderPage(doc)) }) }
            }
        }
    }
    val waiting = session.obligations?.waitingOn.orEmpty()
    val reviews = waiting.filter { it.kind == "review" }
    val reviewHistory = session.obligations?.reviewHistory.orEmpty()
    if (reviews.isNotEmpty() || reviewHistory.isNotEmpty()) {
        Surface(color = Violet.copy(alpha = 0.07f), shape = RoundedCornerShape(12.dp)) {
            Column(Modifier.fillMaxWidth().padding(14.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text("Reviews", style = MaterialTheme.typography.titleSmall, color = Violet)
                if (reviews.isEmpty()) Text("No reviews pending", style = MaterialTheme.typography.bodySmall, color = TextMuted)
                reviews.forEach { review ->
                    ActivityDetail("Waiting for ${review.label}", "${review.state.replace('_', ' ')} · ${relativeSummaryAge(review.since)}", Violet)
                    if (review.lastError != null) AgentDisclosure("Review check needs attention", "View details") {
                        Text(review.lastError, style = MaterialTheme.typography.bodySmall, color = Amber)
                    }
                }
                if (reviewHistory.isNotEmpty()) AgentDisclosure("Review history", "${reviewHistory.sumOf { it.landedCount }} reviews received · ${reviewHistory.size} pull requests") {
                    reviewHistory.forEach { review ->
                        ActivityDetail("${review.repo.substringAfterLast('/')} #${review.prNumber}", "${review.landedCount} received · ${review.landedRequestedByAgent} of ${review.requestedByAgent} requested by this agent received", Violet)
                    }
                }
            }
        }
    }
    val activeJobs = session.jobs.filter { it.state in listOf("pending", "running") }
    if (waiting.any { it.kind != "review" } || activeJobs.isNotEmpty()) {
        Surface(color = Cyan.copy(alpha = 0.06f), shape = RoundedCornerShape(12.dp)) {
            Column(Modifier.fillMaxWidth().padding(14.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text(if (isWaitingForResult(session)) "Waiting for results" else "In progress", style = MaterialTheme.typography.titleSmall, color = Cyan)
                activeJobs.take(2).forEach { job ->
                    FollowableJobDetail(job, jobSummary(job).removePrefix("${job.label} · "), follow)
                }
                if (activeJobs.size > 2 || activeJobs.any { it.holding?.detail != null }) {
                    AgentDisclosure(if (activeJobs.size > 2) "All ${activeJobs.size} jobs" else "Queue details", "${activeJobs.count { it.state == "running" }} running · ${activeJobs.count { it.state == "pending" }} queued") {
                        activeJobs.forEach { job ->
                            FollowableJobDetail(job, listOfNotNull(jobSummary(job).removePrefix("${job.label} · "), job.holding?.detail).distinct().joinToString("\n"), follow)
                        }
                    }
                }
                waiting.filter { it.kind != "review" && (it.kind != "queue_job" || activeJobs.isEmpty()) }.forEach { wait ->
                    ActivityDetail(wait.label, "${wait.state.replace('_', ' ')} · ${ageFromIso(wait.since)}", Cyan)
                    if (wait.lastError != null || wait.lastPolledAt != null) AgentDisclosure("Check details", wait.lastPolledAt?.let { "Last checked ${ageFromIso(it)} ago" } ?: "Check needs attention") {
                        Text(wait.lastError ?: "Waiting for the next result.", style = MaterialTheme.typography.bodySmall, color = if (wait.lastError != null) Amber else TextSecondary)
                    }
                }
            }
        }
    }
    val finished = session.jobs.filter { it.state !in listOf("pending", "running") }
    if (finished.isNotEmpty()) AgentDisclosure("Recent job history", "${finished.size} finished jobs") {
        finished.forEach { job -> ActivityDetail(job.label, jobSummary(job).removePrefix("${job.label} · "), if (job.exitCode == null || job.exitCode == 0) TextSecondary else Rose) }
    }
}

/** "Work  Ticket #1452 · PR #1470": each claim opens its ticket page in the reader. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun WorkLine(claims: List<SessionClaim>, onOpenPage: (ReaderPage) -> Unit) {
    val withRepo = claims.map { it.repo }.distinct().size > 1
    Surface(color = Cyan.copy(alpha = 0.06f), shape = RoundedCornerShape(12.dp)) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 14.dp, vertical = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text("Work", style = MaterialTheme.typography.titleSmall, color = Cyan)
            FlowRow(verticalArrangement = Arrangement.Center) {
                claims.forEachIndexed { index, claim ->
                    if (index > 0) Text(" · ", style = MaterialTheme.typography.bodyMedium, color = TextMuted, modifier = Modifier.padding(vertical = 8.dp))
                    val label = workClaimLabel(claim, withRepo)
                    Text(
                        text = label,
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurface,
                        fontFamily = FontFamily.Monospace,
                        textDecoration = TextDecoration.Underline,
                        modifier = Modifier
                            .clickable { onOpenPage(ownerReaderPage(claim.title.ifBlank { label }, claimHistoryPath(claim))) }
                            .padding(vertical = 8.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun DocRow(doc: SessionDoc, onClick: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth().clickable(onClick = onClick).padding(horizontal = 14.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(doc.title.ifBlank { docDisplayName(doc) }, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurface, maxLines = 2, overflow = TextOverflow.Ellipsis)
            Text("Published ${relativeSummaryAge(doc.publishedAt).replaceFirstChar { it.lowercaseChar() }}", style = MaterialTheme.typography.bodySmall, color = TextMuted, maxLines = 1)
            if (doc.reviewUndelivered) Text("Review not delivered: author retired", style = MaterialTheme.typography.bodySmall, color = Amber, maxLines = 1)
        }
        StatusChip(label = docStateLabel(doc.state), tint = docStateTint(doc.state))
    }
}

private fun docStateTint(state: String): Color = when (state) {
    "new" -> Cyan
    "updated" -> Amber
    "review_requested" -> Fuchsia
    "reviewed" -> Emerald
    else -> TextMuted
}

@Composable
private fun WhatSummarySection(
    state: WhatUiState,
    onUpdate: () -> Unit,
    onRegenerate: () -> Unit,
) {
    val busy = state.status == "pending" || state.status == "running"
    HorizontalDivider(color = Border)
    Column(
        modifier = Modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            Text(
                text = "Summary",
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
            )
            if (busy) {
                StatusChip(
                    label = if (state.activeMode == WhatRequestMode.Update) "updating" else "summarizing",
                    tint = Cyan,
                )
            }
        }

        state.entries.forEachIndexed { index, entry ->
            if (index > 0) {
                HorizontalDivider(color = Border)
            }
            Text(
                text = "${if (entry.isUpdate) "Update" else "Summary"} · ${relativeSummaryAge(entry.createdAt)}",
                style = MaterialTheme.typography.labelMedium, color = TextMuted,
            )
            MarkdownText(entry.markdown)
        }

        if (busy) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                CircularProgressIndicator(
                    modifier = Modifier.size(18.dp),
                    strokeWidth = 2.dp,
                    color = Cyan,
                )
                Text(
                    text = if (state.activeMode == WhatRequestMode.Update) {
                        "Checking what changed..."
                    } else {
                        "Generating a fresh summary..."
                    },
                    color = TextSecondary,
                    style = MaterialTheme.typography.bodySmall,
                )
            }
        }

        state.error?.takeIf { state.status == "failed" || state.status == "timed_out" }?.let { error ->
            Text(
                text = error,
                color = Rose,
                style = MaterialTheme.typography.bodySmall,
            )
        }

        if (!busy) {
            Row(
                modifier = Modifier.horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (state.entries.isNotEmpty()) {
                    ActionPill(
                        label = "Update",
                        icon = Icons.Rounded.Refresh,
                        onClick = onUpdate,
                        tint = Cyan,
                    )
                }
                ActionPill(
                    label = if (state.entries.isEmpty()) "Try again" else "Regenerate",
                    icon = Icons.Rounded.RestartAlt,
                    onClick = onRegenerate,
                    tint = Violet,
                )
            }
        }
    }
}

@Composable
private fun MarkdownText(markdown: String) {
    val context = androidx.compose.ui.platform.LocalContext.current
    val markwon = remember(context) { io.noties.markwon.Markwon.create(context) }
    val textColor = MaterialTheme.colorScheme.onSurface.toArgb()
    val linkColor = Cyan.toArgb()
    androidx.compose.ui.viewinterop.AndroidView(
        modifier = Modifier.fillMaxWidth(),
        factory = { viewContext ->
            android.widget.TextView(viewContext).apply {
                setTextIsSelectable(true)
                movementMethod = android.text.method.LinkMovementMethod.getInstance()
                setLineSpacing(0f, 1.15f)
                setPadding(0, 0, 0, 0)
                textSize = 15f
            }
        },
        update = { textView ->
            textView.setTextColor(textColor)
            textView.setLinkTextColor(linkColor)
            markwon.setMarkdown(textView, markdown)
        },
    )
}

@Composable
private fun StatusChip(label: String, tint: Color) {
    Surface(shape = RoundedCornerShape(999.dp), color = tint.copy(alpha = 0.18f), border = androidx.compose.foundation.BorderStroke(1.dp, tint.copy(alpha = 0.32f))) {
        Text(label.uppercase(), modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp), style = MaterialTheme.typography.labelSmall, color = tint)
    }
}

@Composable
private fun ActionPill(label: String, icon: androidx.compose.ui.graphics.vector.ImageVector, onClick: () -> Unit, tint: Color = Emerald) {
    AssistChip(
        onClick = onClick,
        label = { Text(label, maxLines = 1) },
        leadingIcon = { Icon(icon, contentDescription = null, tint = tint) },
        colors = AssistChipDefaults.assistChipColors(containerColor = PanelMuted, labelColor = MaterialTheme.colorScheme.onSurface),
        border = AssistChipDefaults.assistChipBorder(enabled = true, borderColor = tint.copy(alpha = 0.32f)),
    )
}

@Composable
private fun InlineBadge(label: String, tint: Color) {
    Surface(shape = RoundedCornerShape(999.dp), color = tint.copy(alpha = 0.16f)) {
        Text(label.uppercase(), modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp), style = MaterialTheme.typography.labelSmall, color = tint)
    }
}

@Composable
private fun EmptyState(query: String, filter: String) {
    Card(colors = CardDefaults.cardColors(containerColor = Panel), border = androidx.compose.foundation.BorderStroke(1.dp, Border)) {
        Column(modifier = Modifier.fillMaxWidth().padding(32.dp), horizontalAlignment = Alignment.CenterHorizontally) {
            Text("No sessions matched", style = MaterialTheme.typography.titleLarge, color = MaterialTheme.colorScheme.onSurface)
            Spacer(Modifier.height(8.dp))
            Text(
                text = if (query.isNotBlank() || filter != "all") "Adjust the filter or search query." else "Waiting for session-manager to report sessions.",
                style = MaterialTheme.typography.bodyMedium,
                color = TextSecondary,
            )
        }
    }
}

private fun telegramLink(session: ClientSession): String? {
    val chatId = session.telegramChatId ?: return null
    val threadId = session.telegramThreadId ?: return null
    val normalizedChatId = chatId.toString().removePrefix("-").removePrefix("100")
    return "https://t.me/c/$normalizedChatId/$threadId"
}

private enum class TreeSlice {
    Active,
    Idle,
}

private fun nodeMatchesSlice(node: WatchSessionNode, slice: TreeSlice): Boolean {
    return when (slice) {
        TreeSlice.Active -> hasActiveBranch(node)
        TreeSlice.Idle -> hasIdleBranch(node)
    }
}

private fun shouldRenderNode(node: WatchSessionNode, slice: TreeSlice): Boolean {
    return when (slice) {
        TreeSlice.Active -> hasActiveBranch(node)
        TreeSlice.Idle -> !isActiveSession(node.session) && hasIdleBranch(node)
    }
}

private fun sliceSections(sections: List<WatchSection>, slice: TreeSlice): List<WatchSection> {
    return sections.mapNotNull { section ->
        val roots = section.roots
            .filter { nodeMatchesSlice(it, slice) }
            .sortedWith(
                compareByDescending<WatchSessionNode> { nodeSliceFreshness(it, slice) }
                    .thenBy { sessionDisplayName(it.session).lowercase() }
                    .thenBy { it.session.id }
            )
        if (roots.isEmpty()) null else section.copy(roots = roots)
    }.sortedWith(
        compareByDescending<WatchSection> { sectionSliceFreshness(it, slice) }
            .thenBy { it.repoLabel.lowercase() }
            .thenBy { it.repoKey }
    )
}

private fun sectionSliceFreshness(section: WatchSection, slice: TreeSlice): Long {
    return section.roots.maxOfOrNull { nodeSliceFreshness(it, slice) } ?: Long.MIN_VALUE
}

private fun nodeSliceFreshness(node: WatchSessionNode, slice: TreeSlice): Long {
    val ownFreshness = when (slice) {
        TreeSlice.Active -> if (isActiveSession(node.session)) sessionLastActivityEpoch(node.session) else Long.MIN_VALUE
        TreeSlice.Idle -> if (!isActiveSession(node.session)) sessionLastActivityEpoch(node.session) else Long.MIN_VALUE
    }
    val childFreshness = node.sameRepoChildren.maxOfOrNull { nodeSliceFreshness(it, slice) } ?: Long.MIN_VALUE
    val crossRepoFreshness = node.crossRepoGroups
        .flatMap { it.children }
        .maxOfOrNull { nodeSliceFreshness(it, slice) } ?: Long.MIN_VALUE
    return maxOf(ownFreshness, childFreshness, crossRepoFreshness)
}

private fun sessionLastActivityEpoch(session: ClientSession): Long {
    return parseIso(session.lastActivity)?.toEpochSecond() ?: Long.MIN_VALUE
}

private fun statusDot(session: ClientSession): Color = if (isWaitingForResult(session)) Cyan else when (sessionVisualState(session)) {
    SessionVisualState.Active -> Emerald
    SessionVisualState.Stopped -> Rose
    SessionVisualState.Inactive -> TextMuted
}

private fun statusTint(session: ClientSession): Color = when (sessionVisualState(session)) {
    SessionVisualState.Active -> Emerald
    SessionVisualState.Stopped -> Rose
    SessionVisualState.Inactive -> TextSecondary
}

private fun activityTint(state: String?): Color = when (activityLabel(state)) {
    "working" -> Emerald
    "thinking" -> Cyan
    "bg-wait" -> Violet
    "waiting" -> Amber
    "stopped" -> Rose
    else -> TextSecondary
}

private fun providerTint(provider: String?): Color = when (provider) {
    "codex-fork" -> Cyan
    "claude" -> Fuchsia
    "codex-app" -> Violet
    else -> TextSecondary
}

@Composable
private fun relativeSummaryAge(timestamp: String?): String {
    var now by remember { mutableStateOf(java.time.OffsetDateTime.now()) }
    LaunchedEffect(timestamp) {
        while (true) { now = java.time.OffsetDateTime.now(); kotlinx.coroutines.delay(60_000) }
    }
    return summaryAgeLabel(timestamp, now)
}
