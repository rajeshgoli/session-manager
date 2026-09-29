package li.rajeshgo.sm.ui.guestbook

import android.widget.Toast
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Close
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import java.time.Duration
import java.time.OffsetDateTime
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import li.rajeshgo.sm.data.model.GuestbookClaim
import li.rajeshgo.sm.data.model.GuestbookEntry
import li.rajeshgo.sm.ui.navigation.AppMenuActions
import li.rajeshgo.sm.ui.navigation.AppTopBar
import li.rajeshgo.sm.ui.navigation.Routes
import li.rajeshgo.sm.ui.queue.shortDuration
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.theme.Violet
import li.rajeshgo.sm.ui.watch.DocReaderOverlay
import li.rajeshgo.sm.ui.watch.MarkdownText
import li.rajeshgo.sm.ui.watch.ownerReaderPage

/** Fetch the next page when this many entries remain below the last visible one. */
private const val PREFETCH_ENTRIES = 3

private val signedFormat = DateTimeFormatter.ofPattern("MMM d, HH:mm")

/**
 * The Guestbook (sm#1660): agents' notes as they finished, newest first,
 * paging older as the list scrolls. A repo chip filters to that repo; a
 * ticket or PR opens its ticket page in the reader.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun GuestbookScreen(
    onBack: () -> Unit,
    menu: AppMenuActions,
    viewModel: GuestbookViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val context = LocalContext.current
    val listState = rememberLazyListState()
    var openClaim by remember { mutableStateOf<GuestbookClaim?>(null) }
    val now = remember(state.entries) { OffsetDateTime.now() }

    LaunchedEffect(Unit) { if (state.entries.isEmpty()) viewModel.refresh() }
    // Infinite scroll: near the end of what is loaded, fetch the next older page.
    val nearEnd by remember {
        derivedStateOf {
            val last = listState.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0
            last >= listState.layoutInfo.totalItemsCount - 1 - PREFETCH_ENTRIES
        }
    }
    LaunchedEffect(nearEnd, state.nextBefore, state.loading) {
        if (nearEnd && !state.loading) viewModel.loadMore()
    }
    // A new filter starts at the top.
    LaunchedEffect(state.repo) { listState.scrollToItem(0) }

    Box(
        modifier = Modifier
            .fillMaxSize().statusBarsPadding().navigationBarsPadding()
            .background(MaterialTheme.colorScheme.background),
    ) {
        PullToRefreshBox(
            isRefreshing = state.refreshing,
            onRefresh = { viewModel.refresh(pull = true) },
            modifier = Modifier.fillMaxSize(),
        ) {
            LazyColumn(
                state = listState,
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(16.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                item(key = "top") {
                    AppTopBar(
                        title = "Guestbook",
                        menu = menu,
                        subtitle = state.repo?.let { "repo: $it" } ?: "What agents said as they finished",
                        busy = state.refreshing,
                        current = Routes.GUESTBOOK,
                        onBack = onBack,
                        onRefresh = { viewModel.refresh(pull = true) },
                    )
                }
                item(key = "filter") {
                    RepoFilter(repo = state.repo, onRepo = viewModel::setRepo)
                    if (state.signedOut) Text("Sign in to load the Guestbook", color = Rose)
                    if (state.entries.isEmpty()) {
                        state.error?.let { Text(it, color = Amber, style = MaterialTheme.typography.bodySmall) }
                    }
                }
                if (state.loading && state.entries.isEmpty()) {
                    item(key = "loading") { Spinner() }
                } else if (state.entries.isEmpty() && !state.signedOut && state.error == null) {
                    item(key = "empty") {
                        Text(
                            if (state.repo != null) "No entries for ${state.repo}."
                            else "No entries yet. Agents sign with sm task-complete --sign-guestbook as they finish.",
                            color = TextMuted,
                            modifier = Modifier.padding(vertical = 24.dp),
                        )
                    }
                }
                items(state.entries, key = { it.id }) { entry ->
                    GuestbookCard(
                        entry = entry,
                        now = now,
                        onRepo = { viewModel.setRepo(repoName(it)) },
                        onClaim = { openClaim = it },
                    )
                }
                if (state.entries.isNotEmpty()) {
                    item(key = "footer") {
                        when {
                            state.loadingMore -> Spinner()
                            state.error != null -> Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
                                Text(state.error.orEmpty(), color = Amber, style = MaterialTheme.typography.bodySmall)
                                TextButton(onClick = viewModel::retryMore) { Text("Retry") }
                            }
                            state.nextBefore == null -> Text(
                                "The first entry.",
                                color = TextMuted,
                                style = MaterialTheme.typography.labelSmall,
                                modifier = Modifier.fillMaxWidth().padding(vertical = 12.dp),
                            )
                        }
                    }
                }
            }
        }

        openClaim?.let { claim ->
            val page = ownerReaderPage("Ticket", guestbookClaimPath(claim))
            key(page) {
                DocReaderOverlay(
                    page = page,
                    loadAuth = viewModel::docReaderAuth,
                    onClose = { openClaim = null },
                    onCopyLink = { link ->
                        val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                        clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm link", link))
                        Toast.makeText(context, "Link copied", Toast.LENGTH_SHORT).show()
                    },
                )
            }
        }
    }
}

@Composable
private fun Spinner() {
    Box(Modifier.fillMaxWidth().padding(24.dp), contentAlignment = Alignment.Center) {
        CircularProgressIndicator(color = Cyan)
    }
}

/** The repo filter: a field while none is set, the active repo as a clearable chip once set. */
@Composable
private fun RepoFilter(repo: String?, onRepo: (String?) -> Unit) {
    if (repo != null) {
        Surface(
            shape = RoundedCornerShape(999.dp),
            color = Cyan.copy(alpha = 0.12f),
            border = BorderStroke(1.dp, Cyan.copy(alpha = 0.32f)),
        ) {
            Row(Modifier.padding(start = 12.dp), verticalAlignment = Alignment.CenterVertically) {
                Text("repo: $repo", color = Cyan, style = MaterialTheme.typography.labelMedium)
                IconButton(onClick = { onRepo(null) }, modifier = Modifier.height(32.dp)) {
                    Icon(Icons.Rounded.Close, contentDescription = "Clear repo filter", tint = Cyan)
                }
            }
        }
        return
    }
    var draft by remember { mutableStateOf("") }
    val focus = LocalFocusManager.current
    OutlinedTextField(
        value = draft,
        onValueChange = { draft = it },
        modifier = Modifier.fillMaxWidth(),
        placeholder = { Text("Filter by repo") },
        singleLine = true,
        shape = RoundedCornerShape(14.dp),
        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
        keyboardActions = KeyboardActions(onSearch = {
            focus.clearFocus()
            onRepo(draft)
        }),
    )
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun GuestbookCard(
    entry: GuestbookEntry,
    now: OffsetDateTime,
    onRepo: (String) -> Unit,
    onClaim: (GuestbookClaim) -> Unit,
) {
    Surface(
        shape = RoundedCornerShape(18.dp),
        color = Panel,
        border = BorderStroke(1.dp, Border),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(horizontal = 14.dp, vertical = 12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    entry.sessionName.ifBlank { entry.sessionId.take(8) },
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                    color = MaterialTheme.colorScheme.onSurface,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f, fill = false),
                )
                Text(
                    "  " + entry.sessionId.take(8),
                    style = MaterialTheme.typography.labelSmall,
                    fontFamily = FontFamily.Monospace,
                    color = TextMuted,
                    maxLines = 1,
                )
                Box(Modifier.weight(1f))
                Text(signedLabel(entry.signedAt, now), style = MaterialTheme.typography.labelSmall, color = TextMuted, maxLines = 1)
            }
            FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Pill(guestbookModelLabel(entry), Violet)
                entry.repos.forEach { repo -> Pill(repoName(repo), Cyan, onClick = { onRepo(repo) }) }
            }
            entry.claims.forEach { claim ->
                Row(verticalAlignment = Alignment.CenterVertically) {
                    TextButton(
                        onClick = { onClaim(claim) },
                        contentPadding = PaddingValues(horizontal = 4.dp, vertical = 0.dp),
                        modifier = Modifier.height(28.dp),
                    ) {
                        Text(guestbookClaimLabel(claim), color = Cyan, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.labelMedium)
                    }
                    Text(
                        claim.title,
                        style = MaterialTheme.typography.bodySmall,
                        color = TextSecondary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            HorizontalDivider(color = Border)
            MarkdownText(entry.text)
        }
    }
}

@Composable
private fun Pill(label: String, tint: Color, onClick: (() -> Unit)? = null) {
    val shape = RoundedCornerShape(999.dp)
    val content: @Composable () -> Unit = {
        Text(
            label,
            modifier = Modifier.padding(horizontal = 10.dp, vertical = 4.dp),
            style = MaterialTheme.typography.labelSmall,
            color = tint,
            maxLines = 1,
        )
    }
    if (onClick != null) {
        Surface(onClick = onClick, shape = shape, color = tint.copy(alpha = 0.12f), border = BorderStroke(1.dp, tint.copy(alpha = 0.24f)), content = content)
    } else {
        Surface(shape = shape, color = tint.copy(alpha = 0.12f), border = BorderStroke(1.dp, tint.copy(alpha = 0.24f)), content = content)
    }
}

/** "Sep 29, 13:53 · 2h ago" in local time; the raw value if it does not parse. */
fun signedLabel(signedAt: String, now: OffsetDateTime): String = runCatching {
    val at = OffsetDateTime.parse(signedAt)
    val local = at.atZoneSameInstant(ZoneId.systemDefault())
    "${signedFormat.format(local)} · ${shortDuration(Duration.between(at, now).seconds)} ago"
}.getOrDefault(signedAt)
