package li.rajeshgo.sm.ui.inbox

import android.app.Application
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.ArrowBack
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.compose.viewModel
import java.net.URLDecoder
import java.time.OffsetDateTime
import java.util.UUID
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import li.rajeshgo.sm.data.model.InboxSendRequest
import li.rajeshgo.sm.data.model.DocAskRequest
import li.rajeshgo.sm.data.model.DocAskTarget
import li.rajeshgo.sm.data.model.InboxThread
import li.rajeshgo.sm.data.model.InboxThreadItem
import li.rajeshgo.sm.data.repository.SessionManagerAuthException
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.links.LinkChip
import li.rajeshgo.sm.ui.links.LinksRow
import li.rajeshgo.sm.ui.queue.secondsBetween
import li.rajeshgo.sm.ui.queue.shortDuration
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.CyanDeep
import li.rajeshgo.sm.ui.theme.Fuchsia
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.PanelMuted
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.watch.DocReaderAuth
import li.rajeshgo.sm.ui.watch.DocReaderOverlay
import li.rajeshgo.sm.ui.watch.MarkdownText
import li.rajeshgo.sm.ui.watch.ReaderPage
import li.rajeshgo.sm.ui.watch.loadDocReaderAuth

/** What a native thread opens on: a thread key (an Inbox row), else an agent's current thread. */
data class ThreadTarget(val key: String?, val sessionId: String?, val title: String, val docId: String? = null, val quote: String? = null, val foldedBy: String? = null)

/**
 * The thread an sm page path names: `/inbox/thread/{key}` or `/inbox/agent/{id}`
 * (what message notifications carry); null for anything else.
 */
fun threadTargetForPath(path: String, title: String): ThreadTarget? {
    val bare = path.substringBefore('?').substringBefore('#')
    bare.removePrefix("/inbox/thread/").takeIf { it != bare && it.isNotBlank() && '/' !in it }?.let {
        return ThreadTarget(URLDecoder.decode(it, "UTF-8"), null, title)
    }
    bare.removePrefix("/inbox/agent/").takeIf { it != bare && it.isNotBlank() && '/' !in it }?.let {
        return ThreadTarget(null, it, title)
    }
    return null
}

/** The work a thread key names, as a GitHub link and its chip text: `ticket:o/r#7` gives "#7 ↗". */
fun threadWorkLink(key: String): Pair<String, String>? {
    val (kind, rest) = key.split(':', limit = 2).takeIf { it.size == 2 }?.let { it[0] to it[1] } ?: return null
    val repo = rest.substringBeforeLast('#', "")
    val number = rest.substringAfterLast('#', "").toLongOrNull() ?: return null
    if (repo.isBlank()) return null
    return when (kind) {
        "ticket" -> "#$number ↗" to "https://github.com/$repo/issues/$number"
        "pr" -> "PR #$number ↗" to "https://github.com/$repo/pull/$number"
        else -> null
    }
}

/** The agents in a thread with an open question, newest first: who "✓ Answered" clears. */
fun threadAskers(thread: InboxThread): List<String> =
    thread.items.filter { it.kind == "message" && it.needsYou }.mapNotNull { it.sender?.id }.distinct().reversed()

/** The agent the thread's ⌨ opens: the reply target, else the newest sender. */
fun threadTerminalAgent(thread: InboxThread): Pair<String, String>? {
    val replyTo = thread.replyTo
    if (replyTo?.id != null) return replyTo.id to (replyTo.name ?: replyTo.id)
    return thread.items.lastOrNull { it.sender != null }?.sender?.let { it.id to it.name }
}

/** Ask shows the doc's conversation starting with its first published revision. */
fun docAskThread(thread: InboxThread, firstPublishedAt: String): InboxThread =
    if (firstPublishedAt.isBlank()) thread else {
        val firstSecond = runCatching { OffsetDateTime.parse(firstPublishedAt).toEpochSecond() }.getOrNull() ?: return thread
        thread.copy(items = thread.items.filter { item ->
            val itemSecond = runCatching { OffsetDateTime.parse(item.at).toEpochSecond() }.getOrNull()
            itemSecond == null || itemSecond >= firstSecond
        })
    }

/** Match the server's live-first reply choice even when an ended sender forwards to that agent. */
fun defaultReplyOptionId(thread: InboxThread): String? =
    thread.replyOptions.firstOrNull { it.status == "live" && it.canSend }?.id
        ?: thread.replyOptions.firstOrNull { it.canSend }?.id

data class ThreadUiState(
    val thread: InboxThread? = null,
    val loading: Boolean = true,
    val error: String? = null,
    val sending: Boolean = false,
    val busy: Boolean = false,
    val askTarget: DocAskTarget? = null,
)

class ThreadViewModel(application: Application) : AndroidViewModel(application) {
    private val settingsRepository = SettingsRepository(application)
    private val repository = SessionManagerRepository(settingsRepository)
    private val _uiState = MutableStateFlow(ThreadUiState())
    val uiState: StateFlow<ThreadUiState> = _uiState
    private var target: ThreadTarget? = null

    private suspend fun credentials(): Pair<String, String>? {
        val serverUrl = settingsRepository.serverUrl.first()
        val token = settingsRepository.accessToken.first()
        if (serverUrl.isBlank() || token.isBlank()) {
            _uiState.update { it.copy(loading = false, error = "Sign in to read the Inbox") }
            return null
        }
        return serverUrl to token
    }

    suspend fun docReaderAuth(): DocReaderAuth? = loadDocReaderAuth(settingsRepository)

    fun open(target: ThreadTarget) {
        if (this.target == target) return
        this.target = target
        _uiState.value = ThreadUiState()
        load()
    }

    /** Reads the thread; once read, it is read by its own key (an agent's thread can be ticket-keyed). */
    fun load() {
        val target = target ?: return
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch
            runCatching {
                val askTarget = target.docId?.let { repository.fetchDocAskTarget(url, token, it) }
                val key = askTarget?.threadKey ?: _uiState.value.thread?.threadKey ?: target.key
                val thread = repository.fetchInboxThread(url, token, key, target.sessionId)
                (askTarget?.let { docAskThread(thread, it.firstPublishedAt) } ?: thread) to askTarget
            }
                .onSuccess { (thread, askTarget) -> _uiState.update { it.copy(thread = thread, askTarget = askTarget, loading = false, error = null) } }
                .onFailure { error ->
                    if (error is CancellationException) throw error
                    if (error is SessionManagerAuthException) settingsRepository.clearAuth()
                    _uiState.update { it.copy(loading = false, error = error.message ?: "Couldn't load the thread") }
                }
        }
    }

    /** Sends [text] to the thread's agent; [onResult] gets the failure's text, or null. */
    fun send(text: String, submissionId: String, recipient: String?, quote: String?, onResult: (String?) -> Unit) {
        val key = _uiState.value.thread?.threadKey ?: return
        if (_uiState.value.sending || text.isBlank()) return
        _uiState.update { it.copy(sending = true) }
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch _uiState.update { it.copy(sending = false) }
            val result = target?.docId?.let { docId ->
                repository.askDoc(url, token, docId, DocAskRequest(text.trim(), quote, recipient ?: _uiState.value.askTarget?.default ?: "reader"))
            } ?: repository.sendInboxThread(url, token, key, InboxSendRequest(submissionId, text.trim(), recipient))
            _uiState.update { it.copy(sending = false) }
            onResult(result.exceptionOrNull()?.let { it.message ?: "Send failed" })
            if (result.isSuccess) {
                load()
            }
        }
    }

    /** Done: the thread leaves Open; [onResult] gets the failure's text, or null. */
    fun done(onResult: (String?) -> Unit) {
        val key = _uiState.value.thread?.threadKey ?: return
        write(onResult) { url, token -> repository.markInboxDone(url, token, key) }
    }

    fun archive(unarchive: Boolean, onResult: (String?) -> Unit) {
        val thread = _uiState.value.thread ?: return
        write(onResult) { url, token -> repository.archiveInbox(url, token, thread.threadKey, unarchive) }
    }

    /** ✓ Answered (spec 1782 C4): each agent with an open question here stops waiting on you. */
    fun answered(onResult: (String?) -> Unit) {
        val askers = _uiState.value.thread?.let(::threadAskers).orEmpty()
        if (askers.isEmpty()) return
        write(onResult) { url, token ->
            askers.map { repository.answerNeedsYou(url, token, it) }.firstOrNull { it.isFailure } ?: Result.success(Unit)
        }
    }

    private fun write(onResult: (String?) -> Unit, block: suspend (String, String) -> Result<*>) {
        if (_uiState.value.busy) return
        _uiState.update { it.copy(busy = true) }
        viewModelScope.launch {
            val (url, token) = credentials() ?: return@launch _uiState.update { it.copy(busy = false) }
            val result = block(url, token)
            _uiState.update { it.copy(busy = false) }
            onResult(result.exceptionOrNull()?.let { it.message ?: "Request failed" })
            if (result.isSuccess) load()
        }
    }
}

/**
 * A work thread drawn natively (spec 1782 J3): messages, your replies, the
 * agent's turns and events as bubbles, a reply box, and a header with the
 * thread's links, ✓ Answered and Done. Back closes it.
 */
@Composable
fun ThreadScreen(
    target: ThreadTarget,
    onClose: () -> Unit,
    onOpenTerminal: (String) -> Unit,
    onDone: () -> Unit,
    onMessage: (String) -> Unit,
) {
    val viewModel: ThreadViewModel = viewModel(key = "thread:${target.key ?: target.sessionId}")
    LaunchedEffect(target) { viewModel.open(target) }
    LaunchedEffect(target) {
        while (isActive) { delay(10_000); viewModel.load() }
    }
    val state by viewModel.uiState.collectAsState()
    val uriHandler = LocalUriHandler.current
    var reader by remember { mutableStateOf<ReaderPage?>(null) }
    var draft by rememberSaveable(target) { mutableStateOf("") }
    // One id per draft: a retried Send of the same text is delivered once.
    var submissionId by rememberSaveable(target) { mutableStateOf(UUID.randomUUID().toString()) }
    // The text last sent under [submissionId]: an edit after it is a new message, with a new id.
    var attempted by rememberSaveable(target) { mutableStateOf<String?>(null) }
    var recipient by rememberSaveable(target) { mutableStateOf<String?>(null) }
    var quote by rememberSaveable(target) { mutableStateOf(target.quote) }
    val listState = rememberLazyListState()
    val thread = state.thread
    val now = remember(thread) { OffsetDateTime.now() }

    BackHandler(enabled = reader == null, onBack = onClose)
    LaunchedEffect(thread?.items?.size) {
        val count = thread?.items?.size ?: 0
        if (count > 0) listState.scrollToItem(count - 1)
    }

    Box(Modifier.fillMaxSize()) {
    Surface(color = MaterialTheme.colorScheme.background, modifier = Modifier.fillMaxSize()) {
        Column(Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding().imePadding()) {
            Row(
                Modifier.fillMaxWidth().padding(start = 4.dp, end = 12.dp, top = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                IconButton(onClick = onClose) {
                    Icon(Icons.AutoMirrored.Rounded.ArrowBack, contentDescription = "Back to the Inbox", tint = TextSecondary)
                }
                Text(
                    thread?.title ?: target.title,
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (thread != null) {
                    TextButton(onClick = { viewModel.archive(target.foldedBy == "archived") { error -> if (error == null) onDone() else onMessage(error) } }, enabled = !state.busy) {
                        Text(if (target.foldedBy == "archived") "Unarchive" else "Archive")
                    }
                    OutlinedButton(
                        onClick = { viewModel.done { error -> if (error == null) onDone() else onMessage(error) } },
                        enabled = !state.busy,
                        contentPadding = PaddingValues(horizontal = 14.dp, vertical = 2.dp),
                        modifier = Modifier.height(34.dp),
                    ) { Text("Done") }
                }
            }
            if (thread != null) {
                val chips = buildList {
                    threadTerminalAgent(thread)?.takeIf { target.docId == null }?.let { (id, name) ->
                        add(LinkChip(name, TextSecondary, onTerminal = { onOpenTerminal(id) }))
                    }
                    threadWorkLink(thread.threadKey)?.let { (text, url) -> add(LinkChip(text, Cyan) { uriHandler.openUri(url) }) }
                }
                Row(
                    Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    LinksRow(chips, Modifier.weight(1f))
                    if (threadAskers(thread).isNotEmpty()) {
                        TextButton(
                            onClick = { viewModel.answered { error -> onMessage(error ?: "Marked answered") } },
                            enabled = !state.busy,
                            contentPadding = PaddingValues(horizontal = 8.dp),
                        ) { Text("✓ Answered", color = Fuchsia) }
                    }
                }
            }
            Box(Modifier.weight(1f).fillMaxWidth()) {
                when {
                    thread == null && state.loading -> CircularProgressIndicator(color = Cyan, modifier = Modifier.align(Alignment.Center))
                    thread == null -> Text(
                        state.error ?: "Thread not found",
                        color = Amber,
                        modifier = Modifier.align(Alignment.Center).padding(24.dp),
                    )
                    else -> LazyColumn(
                        state = listState,
                        modifier = Modifier.fillMaxSize(),
                        contentPadding = PaddingValues(horizontal = 16.dp, vertical = 12.dp),
                        verticalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        itemsIndexed(thread.items, key = { index, item -> item.id ?: "${item.kind}-$index-${item.at}" }) { _, item ->
                            ThreadItem(item, now) { path -> reader = ReaderPage(title = item.text.orEmpty(), subtitle = thread.title, path = path) }
                        }
                    }
                }
            }
            if (thread != null) {
                ReplyBox(
                    thread = thread,
                    draft = draft,
                    sending = state.sending,
                    askTarget = state.askTarget,
                    recipient = recipient,
                    onRecipient = { recipient = it },
                    quote = quote,
                    onRemoveQuote = { quote = null },
                    onDraft = { text ->
                        draft = text
                        if (attempted != null && text != attempted) {
                            submissionId = UUID.randomUUID().toString()
                            attempted = null
                        }
                    },
                    onSend = {
                        attempted = draft
                        viewModel.send(draft, submissionId, recipient, quote) { error ->
                            if (error == null) {
                                draft = ""
                                submissionId = UUID.randomUUID().toString()
                                attempted = null
                                quote = null
                            } else {
                                onMessage(error)
                            }
                        }
                    },
                )
            }
        }
    }

    reader?.let { page ->
        key(page) {
            DocReaderOverlay(
                page = page,
                loadAuth = viewModel::docReaderAuth,
                onClose = { reader = null },
                onCopyLink = {},
            )
        }
    }
    }
}

private fun itemAge(at: String, now: OffsetDateTime): String =
    secondsBetween(at, now)?.let { shortDuration(it) + " ago" }.orEmpty()

@Composable
private fun ThreadItem(item: InboxThreadItem, now: OffsetDateTime, onOpenLink: (String) -> Unit) {
    if (item.type == "doc_revision") {
        Bubble(color = Panel, border = Border, edge = Fuchsia) {
            Text(item.text ?: "Document revision", style = MaterialTheme.typography.titleSmall, fontWeight = FontWeight.Bold)
            Meta(listOfNotNull(item.sender?.name, item.pr?.let { "PR #$it" }, item.sha?.take(7), item.reviewState).joinToString(" · "))
            item.link?.let { link -> Row {
                TextButton(onClick = { onOpenLink(link) }) { Text("Open") }
                TextButton(onClick = { onOpenLink("${link.substringBefore('#')}#sm-review") }) { Text("Review") }
            } }
        }
        return
    }
    when (item.kind) {
        "owner" -> Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.CenterEnd) {
            Bubble(color = CyanDeep, border = null, edge = null) {
                item.quotes.forEach { quote ->
                    if (quote.quote.isNotBlank()) {
                        Row(Modifier.padding(bottom = 4.dp)) {
                            Box(Modifier.width(3.dp).height(18.dp).background(Fuchsia))
                            Text(quote.quote, color = TextSecondary, style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(start = 8.dp))
                        }
                    }
                    if (quote.body.isNotBlank()) Text(quote.body, style = MaterialTheme.typography.bodyMedium)
                }
                if (!item.body.isNullOrBlank()) Text(item.body, style = MaterialTheme.typography.bodyMedium)
                Meta(listOfNotNull("You", itemAge(item.at, now), item.to?.let { "to $it" }).filter { it.isNotBlank() }.joinToString(" · "))
            }
        }
        "event" -> Text(
            listOfNotNull(item.text, itemAge(item.at, now)).joinToString(" · "),
            color = if (item.link != null) Cyan else TextMuted,
            style = MaterialTheme.typography.labelSmall,
            fontFamily = FontFamily.Monospace,
            textAlign = TextAlign.Center,
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp)
                .let { modifier -> item.link?.let { link -> modifier.clickable { onOpenLink(link) } } ?: modifier },
        )
        "turn", "agent_reply" -> Bubble(color = Panel, border = Border, edge = Cyan) {
            Text(
                listOfNotNull(item.sender?.name, if (item.kind == "turn") "Last turn" else "Reply", itemAge(item.at, now)).filter { it.isNotBlank() }.joinToString(" · "),
                color = Cyan,
                style = MaterialTheme.typography.labelSmall,
                fontFamily = FontFamily.Monospace,
                modifier = Modifier.padding(bottom = 4.dp),
            )
            MarkdownText(item.markdown.orEmpty())
        }
        else -> Bubble(color = Panel, border = if (item.needsYou) Amber.copy(alpha = 0.6f) else Border, edge = null) {
            if (item.needsYou) {
                Text(
                    "NEEDS YOU",
                    color = Amber,
                    style = MaterialTheme.typography.labelSmall,
                    fontWeight = FontWeight.Bold,
                    modifier = Modifier.padding(bottom = 4.dp)
                        .background(Amber.copy(alpha = 0.16f), RoundedCornerShape(4.dp))
                        .padding(horizontal = 5.dp, vertical = 1.dp),
                )
            }
            // The title is the body's first line, so it stands in only for an empty body.
            val markdown = item.markdown.orEmpty()
            if (markdown.isBlank() && !item.title.isNullOrBlank()) {
                Text(item.title, style = MaterialTheme.typography.titleSmall, fontWeight = FontWeight.SemiBold)
            }
            if (markdown.isNotBlank()) MarkdownText(markdown)
            Meta(listOfNotNull(item.sender?.name, itemAge(item.at, now), item.state).filter { it.isNotBlank() }.joinToString(" · "))
        }
    }
}

@Composable
private fun Bubble(color: Color, border: Color?, edge: Color?, content: @Composable () -> Unit) {
    Surface(
        color = color,
        shape = RoundedCornerShape(12.dp),
        border = border?.let { BorderStroke(1.dp, it) },
        modifier = Modifier.fillMaxWidth(0.92f),
    ) {
        Column(
            Modifier
                .drawBehind { if (edge != null) drawRect(edge, size = Size(3.dp.toPx(), size.height)) }
                .padding(horizontal = 12.dp, vertical = 10.dp),
        ) { content() }
    }
}

@Composable
private fun Meta(text: String) {
    if (text.isBlank()) return
    Text(text, color = TextMuted, style = MaterialTheme.typography.labelSmall, modifier = Modifier.padding(top = 6.dp))
}

@Composable
private fun ReplyBox(
    thread: InboxThread,
    draft: String,
    sending: Boolean,
    askTarget: DocAskTarget?,
    recipient: String?,
    onRecipient: (String) -> Unit,
    quote: String?,
    onRemoveQuote: () -> Unit,
    onDraft: (String) -> Unit,
    onSend: () -> Unit,
) {
    Surface(color = PanelMuted, shape = RoundedCornerShape(topStart = 12.dp, topEnd = 12.dp)) {
        Column(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp)) {
            if (!quote.isNullOrBlank()) Row(verticalAlignment = Alignment.CenterVertically) {
                Text("“$quote”", modifier = Modifier.weight(1f), maxLines = 3, overflow = TextOverflow.Ellipsis, style = MaterialTheme.typography.bodySmall)
                TextButton(onClick = onRemoveQuote) { Text("Remove") }
            }
            val choices = if (askTarget != null) buildList {
                if (askTarget.author?.live == true) add("author" to askTarget.author.name)
                add("reader" to (askTarget.reader?.name ?: "Reader agent"))
                if (askTarget.author?.restorable == true) add("restore_author" to "Bring back ${askTarget.author.name} · reloads about ${((askTarget.author.contextTokens ?: 0) + 500) / 1000}k tokens")
            } else thread.replyOptions.filter { it.canSend }.map { it.id to (it.name + if (it.restores) " · restores" else "") }
            val chosen = recipient ?: (if (askTarget != null) askTarget.default else defaultReplyOptionId(thread))
            val target = choices.firstOrNull { it.first == chosen }?.second ?: thread.replyTo?.name
            if (choices.isEmpty() && (askTarget == null && !thread.canSend)) {
                Text("No agent is left to reply to.", color = TextMuted, style = MaterialTheme.typography.bodySmall)
                return@Column
            }
            var menuOpen by remember { mutableStateOf(false) }
            if (choices.size > 1) Box {
                TextButton(onClick = { menuOpen = true }) { Text("To: ${target ?: "Select agent"} ▾") }
                DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                    choices.forEach { (id, name) -> DropdownMenuItem(text = { Text(name) }, onClick = { onRecipient(id); menuOpen = false }) }
                }
            } else Text("To: ${target ?: "Reader agent"}", style = MaterialTheme.typography.labelSmall, color = TextSecondary)
            Row(verticalAlignment = Alignment.Bottom) {
                OutlinedTextField(
                    value = draft,
                    onValueChange = onDraft,
                    placeholder = { Text(if (askTarget != null) "Ask about this doc…" else "Write to $target") },
                    enabled = !sending,
                    maxLines = 6,
                    modifier = Modifier.weight(1f),
                )
                TextButton(
                    onClick = onSend,
                    enabled = !sending && draft.isNotBlank(),
                    modifier = Modifier.padding(start = 6.dp),
                ) {
                    if (sending) CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp, color = Cyan)
                    else Text("Send", color = if (draft.isNotBlank()) Cyan else TextMuted, fontWeight = FontWeight.Bold)
                }
            }
            val restores = if (askTarget == null) thread.replyOptions.firstOrNull { it.id == chosen }?.restores ?: thread.replyTo?.restores ?: false else chosen == "restore_author"
            if (restores) {
                Text("Replying brings this agent back.", color = Rose, style = MaterialTheme.typography.labelSmall)
            }
        }
    }
}
