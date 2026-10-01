package li.rajeshgo.sm.data.remote

import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import okhttp3.Interceptor
import okhttp3.Response

/**
 * The latest JSON body of each GET path, so a bug report carries what the
 * screen had loaded (spec 1859 C3). Keyed by path without query; at most
 * [MAX_KEYS] paths, the least recently fetched evicted first.
 */
class PageDataRing(private val clock: () -> Long = System::nanoTime) {
    private class Entry(val body: String, val fetchedAt: Long)

    private val entries = LinkedHashMap<String, Entry>()

    @Synchronized
    fun record(path: String, body: String) {
        entries.remove(path)
        entries[path] = Entry(body, clock())
        while (entries.size > MAX_KEYS) entries.remove(entries.keys.first())
    }

    @Synchronized
    fun paths(): List<String> = entries.keys.toList()

    /**
     * The entries fetched at or after [since], as `{path: body}`. Bodies that
     * are not JSON are left out; while the whole exceeds [MAX_CHARS], the
     * largest entry is dropped.
     */
    fun snapshot(since: Long): JsonObject {
        val fresh = synchronized(this) {
            entries.filterValues { it.fetchedAt >= since }.mapValues { it.value.body }
        }
        val kept = fresh.mapNotNull { (path, body) ->
            runCatching { Json.parseToJsonElement(body) }.getOrNull()?.let { path to it }
        }.toMap().toMutableMap()
        while (kept.isNotEmpty() && JsonObject(kept).toString().length > MAX_CHARS) {
            kept.remove(kept.maxBy { it.value.toString().length }.key)
        }
        return JsonObject(kept as Map<String, JsonElement>)
    }

    companion object {
        const val MAX_KEYS = 12
        const val MAX_CHARS = 300_000
    }
}

/** Records successful JSON GET responses into [ring]; every client the app builds carries it. */
class PageDataRecorder(private val ring: PageDataRing) : Interceptor {
    override fun intercept(chain: Interceptor.Chain): Response {
        val response = chain.proceed(chain.request())
        if (chain.request().method == "GET" && response.isSuccessful &&
            response.body?.contentType()?.subtype == "json"
        ) {
            val peeked = runCatching { response.peekBody(PEEK_LIMIT).bytes() }.getOrNull()
            // A body as long as the peek limit was cut off and would not parse.
            if (peeked != null && peeked.size < PEEK_LIMIT) ring.record(chain.request().url.encodedPath, peeked.decodeToString())
        }
        return response
    }

    companion object {
        private const val PEEK_LIMIT = 1L shl 20

        /** The app's one ring. */
        val pages = PageDataRing()
    }
}
