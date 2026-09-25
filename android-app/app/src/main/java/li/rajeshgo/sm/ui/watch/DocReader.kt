package li.rajeshgo.sm.ui.watch

import android.annotation.SuppressLint
import android.content.Intent
import android.graphics.Bitmap
import android.net.Uri
import android.webkit.ClientCertRequest
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.rounded.Link
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import java.net.URI
import li.rajeshgo.sm.data.model.SessionClaim
import li.rajeshgo.sm.data.model.SessionDoc
import li.rajeshgo.sm.data.remote.DeviceClientCertificate
import li.rajeshgo.sm.ui.theme.BorderStrong
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Panel
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted

/** What the reader needs to load owner pages (`/docs/`, `/history`, `/t/`) as the app: the API base, the SM device bearer token and the Cloudflare device certificate. */
class DocReaderAuth(
    val serverUrl: String,
    val accessToken: String,
    val clientCertificate: DeviceClientCertificate?,
) {
    val headers: Map<String, String> get() = mapOf("Authorization" to "Bearer $accessToken")
}

private const val DOC_VERSION_LENGTH = 12

fun docStateLabel(state: String): String = when (state) {
    "new" -> "New"
    "updated" -> "Updated"
    "review_requested" -> "Review requested"
    "reviewed" -> "Reviewed"
    "read" -> "Read"
    else -> state.replace('_', ' ').replaceFirstChar { it.uppercaseChar() }
}

/** `<repo-name>/<path in repo>`, the name `sm doc` uses. */
fun docDisplayName(doc: SessionDoc): String =
    doc.name?.takeIf(String::isNotBlank) ?: "${doc.repo.substringAfterLast('/')}/${doc.path}"

/** The readable reader path `/docs/<repo-name>/<path>?version=<sha12>`. Built locally only when an older server sends no `reader_path`; never the id route. */
fun docReaderPath(doc: SessionDoc): String =
    doc.readerPath?.takeIf { it.startsWith("/docs/") }
        ?: readableDocPath(doc.repo, doc.path, doc.latestCommitSha)

fun readableDocPath(repo: String, path: String, commitSha: String): String {
    val encoded = (listOf(repo.substringAfterLast('/')) + path.split('/'))
        .joinToString("/", transform = ::percentEncodeSegment)
    return "/docs/$encoded?version=${commitSha.take(DOC_VERSION_LENGTH)}"
}

private fun percentEncodeSegment(segment: String): String = buildString {
    segment.toByteArray(Charsets.UTF_8).forEach { byte ->
        val char = (byte.toInt() and 0xFF).toChar()
        if (char in 'A'..'Z' || char in 'a'..'z' || char in '0'..'9' || char in "-_.~") append(char)
        else append("%%%02X".format(byte.toInt() and 0xFF))
    }
}

fun readerUrl(serverUrl: String, path: String): String = serverUrl.trim().trimEnd('/') + path

/** The claim's ticket page `/t/<repo-name>/<n>`, built locally when an older server sends no `history_path`. */
fun claimHistoryPath(claim: SessionClaim): String =
    claim.historyPath?.takeIf { it.startsWith("/t/") }
        ?: "/t/${percentEncodeSegment(claim.repo.substringAfterLast('/'))}/${claim.number}"

/** `Ticket #1452` or `PR #1470`; the repo name is added when a session holds work in more than one repo. */
fun workClaimLabel(claim: SessionClaim, withRepo: Boolean): String {
    val kind = if (claim.kind == "pr") "PR" else "Ticket"
    val repo = if (withRepo) "${claim.repo.substringAfterLast('/')} " else ""
    return "$kind $repo#${claim.number}"
}

/** The Work line's claims: tickets first, then PRs, each in claim order. */
fun workClaims(claims: List<SessionClaim>): List<SessionClaim> = claims.sortedBy { if (it.kind == "pr") 1 else 0 }

/** A page the reader opens: a doc, or an owner page such as `/history` or a ticket page. */
data class ReaderPage(
    val title: String,
    val subtitle: String,
    val path: String,
    /** The doc's `browser_url`, which the copy-link button needs; owner pages have none. */
    val browserUrl: String? = null,
    /** Owner pages show the loaded page's own title and path, since the owner moves between them. */
    val followsPage: Boolean = false,
)

fun docReaderPage(doc: SessionDoc): ReaderPage = ReaderPage(
    title = doc.title.ifBlank { docDisplayName(doc) },
    subtitle = docDisplayName(doc),
    path = docReaderPath(doc),
    browserUrl = doc.browserUrl,
)

fun ownerReaderPage(title: String, path: String): ReaderPage =
    ReaderPage(title = title, subtitle = path, path = path, followsPage = true)

val historyReaderPage: ReaderPage get() = ownerReaderPage("History", "/history")

/**
 * The link to share for the page on screen: the same path and query on the
 * owner's browser host (`browser_url`), so it opens in a laptop browser. The
 * app's own API host needs the device certificate, so it is only the fallback.
 */
fun shareUrl(currentUrl: String, browserUrl: String?): String {
    val browserOrigin = browserUrl?.let(::originOf) ?: return currentUrl
    val current = runCatching { URI(currentUrl) }.getOrNull() ?: return currentUrl
    val query = current.rawQuery?.let { "?$it" }.orEmpty()
    val fragment = current.rawFragment?.let { "#$it" }.orEmpty()
    return "$browserOrigin${current.rawPath}$query$fragment"
}

private fun originOf(url: String): String? {
    val uri = runCatching { URI(url) }.getOrNull() ?: return null
    val scheme = uri.scheme ?: return null
    val authority = uri.rawAuthority ?: return null
    return "$scheme://$authority"
}

enum class DocNavigation {
    /** A same-origin owner page: reload it through `loadUrl` with the device auth headers. */
    Reload,

    /** A fragment jump inside the page on screen: let the WebView scroll. */
    InPage,

    /** Anything else (GitHub, external links, other sm routes): the system browser. */
    External,
}

fun docNavigation(serverUrl: String, currentUrl: String?, targetUrl: String): DocNavigation {
    val target = runCatching { URI(targetUrl) }.getOrNull() ?: return DocNavigation.External
    val server = runCatching { URI(serverUrl.trim()) }.getOrNull() ?: return DocNavigation.External
    if (!sameOrigin(server, target) || !isOwnerPagePath(target.rawPath.orEmpty())) {
        return DocNavigation.External
    }
    val current = currentUrl?.let { runCatching { URI(it) }.getOrNull() }
    if (target.rawFragment != null && current != null && withoutFragment(current) == withoutFragment(target)) {
        return DocNavigation.InPage
    }
    return DocNavigation.Reload
}

/**
 * The pages that stay in the reader: docs, History, ticket pages, and the web
 * watch at `/` and `/watch`, so the page shell's Watch · History tabs never
 * leave the authenticated web view.
 */
fun isOwnerPagePath(path: String): Boolean =
    path.startsWith("/docs/") || path.startsWith("/t/") ||
        path == "/history" || path == "/watch" || path == "/" || path.isEmpty()

private fun sameOrigin(a: URI, b: URI): Boolean =
    a.scheme.equals(b.scheme, ignoreCase = true) &&
        a.host != null && a.host.equals(b.host, ignoreCase = true) &&
        effectivePort(a) == effectivePort(b)

private fun effectivePort(uri: URI): Int = when {
    uri.port != -1 -> uri.port
    uri.scheme.equals("https", ignoreCase = true) -> 443
    uri.scheme.equals("http", ignoreCase = true) -> 80
    else -> -1
}

private fun withoutFragment(uri: URI): String = uri.toString().substringBefore('#')

@Composable
fun DocReaderOverlay(
    page: ReaderPage,
    loadAuth: suspend () -> DocReaderAuth?,
    onClose: () -> Unit,
    onCopyLink: (String) -> Unit,
) {
    val auth by produceState<Result<DocReaderAuth?>?>(null, page) { value = runCatching { loadAuth() } }
    // Our own back stack: WebView history navigation would re-request pages
    // without the auth headers, so back reloads the previous URL with them.
    val history = remember(page) { mutableStateListOf<String>() }
    var webViewRef by remember { mutableStateOf<WebView?>(null) }
    var loading by remember { mutableStateOf(true) }
    var loadedTitle by remember(page) { mutableStateOf<String?>(null) }
    val readyAuth = auth?.getOrNull()
    val currentPath = history.lastOrNull()?.let { runCatching { URI(it).rawPath }.getOrNull() }
    val title = if (page.followsPage) loadedTitle ?: page.title else page.title
    val subtitle = if (page.followsPage) currentPath?.takeIf(String::isNotEmpty) ?: page.subtitle else page.subtitle

    BackHandler {
        val webView = webViewRef
        if (readyAuth != null && webView != null && history.size > 1) {
            history.removeAt(history.lastIndex)
            webView.loadUrl(history.last(), readyAuth.headers)
        } else {
            onClose()
        }
    }

    Surface(modifier = Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        // Edge-to-edge windows don't resize for the keyboard: pad the reader
        // so the page (and its comment box) ends above it.
        Column(modifier = Modifier.fillMaxSize().imePadding()) {
            Surface(color = Panel, border = androidx.compose.foundation.BorderStroke(1.dp, BorderStrong)) {
                Row(
                    modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    Column(modifier = Modifier.weight(1f)) {
                        Text(
                            text = title,
                            style = MaterialTheme.typography.titleMedium,
                            color = MaterialTheme.colorScheme.onSurface,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                        Text(
                            text = subtitle,
                            style = MaterialTheme.typography.labelSmall,
                            color = TextMuted,
                            fontFamily = FontFamily.Monospace,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                    if (readyAuth != null && page.browserUrl != null) {
                        IconButton(onClick = {
                            val current = webViewRef?.url ?: history.lastOrNull() ?: readerUrl(readyAuth.serverUrl, page.path)
                            onCopyLink(shareUrl(current, page.browserUrl))
                        }) {
                            Icon(Icons.Rounded.Link, contentDescription = "Copy doc link", tint = Cyan)
                        }
                    }
                    OutlinedButton(
                        onClick = onClose,
                        modifier = Modifier.height(40.dp),
                        contentPadding = PaddingValues(horizontal = 10.dp, vertical = 2.dp),
                        shape = RoundedCornerShape(10.dp),
                    ) { Text("Close") }
                }
            }
            when {
                auth == null -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                    CircularProgressIndicator(color = Cyan)
                }
                readyAuth == null -> Box(Modifier.fillMaxSize().padding(24.dp), contentAlignment = Alignment.Center) {
                    Text(
                        text = auth?.exceptionOrNull()?.message ?: "Sign in to open this page",
                        color = Rose,
                        style = MaterialTheme.typography.bodyMedium,
                    )
                }
                else -> {
                    if (loading) LinearProgressIndicator(modifier = Modifier.fillMaxWidth(), color = Cyan)
                    DocReaderWebView(
                        auth = readyAuth,
                        initialUrl = readerUrl(readyAuth.serverUrl, page.path),
                        history = history,
                        onWebView = { webViewRef = it },
                        onLoading = { loading = it },
                        onTitle = { loadedTitle = it },
                    )
                }
            }
        }
    }
}

@SuppressLint("SetJavaScriptEnabled")
@Composable
private fun DocReaderWebView(
    auth: DocReaderAuth,
    initialUrl: String,
    history: MutableList<String>,
    onWebView: (WebView?) -> Unit,
    onLoading: (Boolean) -> Unit,
    onTitle: (String?) -> Unit,
) {
    var webViewRef by remember { mutableStateOf<WebView?>(null) }
    DisposableEffect(Unit) {
        onDispose {
            webViewRef?.destroy()
            webViewRef = null
            onWebView(null)
        }
    }
    AndroidView(
        modifier = Modifier.fillMaxSize(),
        factory = { context ->
            WebView(context).apply {
                settings.javaScriptEnabled = true
                settings.domStorageEnabled = true
                settings.allowFileAccess = false
                settings.allowContentAccess = false
                settings.useWideViewPort = true
                settings.loadWithOverviewMode = true
                settings.builtInZoomControls = true
                settings.displayZoomControls = false
                webViewClient = object : WebViewClient() {
                    override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                        if (!request.isForMainFrame) return false
                        val target = request.url.toString()
                        return when (docNavigation(auth.serverUrl, history.lastOrNull(), target)) {
                            DocNavigation.InPage -> false
                            DocNavigation.Reload -> {
                                // WebView drops custom headers on page-initiated navigations and
                                // redirects, so every owner page is loaded again with them.
                                if (request.isRedirect && history.isNotEmpty()) history[history.lastIndex] = target
                                else history.add(target)
                                view.loadUrl(target, auth.headers)
                                true
                            }
                            DocNavigation.External -> {
                                runCatching {
                                    context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(target)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                                }
                                true
                            }
                        }
                    }

                    override fun onReceivedClientCertRequest(view: WebView, request: ClientCertRequest) {
                        val certificate = auth.clientCertificate
                        val serverHost = runCatching { URI(auth.serverUrl.trim()).host }.getOrNull()
                        if (certificate != null && request.host.equals(serverHost, ignoreCase = true)) {
                            request.proceed(certificate.privateKey, certificate.certificateChain)
                        } else {
                            request.cancel()
                        }
                    }

                    override fun onPageStarted(view: WebView, url: String?, favicon: Bitmap?) = onLoading(true)

                    override fun onPageFinished(view: WebView, url: String?) {
                        onLoading(false)
                        // WebView reports the URL as the title of a page without one.
                        onTitle(view.title?.takeIf { it.isNotBlank() && it != url })
                    }
                }
                history.clear()
                history.add(initialUrl)
                loadUrl(initialUrl, auth.headers)
                webViewRef = this
                onWebView(this)
            }
        },
    )
}
