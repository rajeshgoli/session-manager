package li.rajeshgo.sm.data.repository

import com.jakewharton.retrofit2.converter.kotlinx.serialization.asConverterFactory
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** The phone's reads of the server's facts, the ✓ route, and the tab cache (spec 1782 C4, J2, J5). */
class WatchStateApiTest {
    @After fun clearCache() = ScreenCache.clear()

    @Test
    fun watchStateAndAnsweredUseTheSpecRoutes() = kotlinx.coroutines.runBlocking {
        val seen = mutableListOf<String>()
        val client = okhttp3.OkHttpClient.Builder().addInterceptor { chain ->
            val request = chain.request()
            val buffer = okio.Buffer()
            request.body?.writeTo(buffer)
            seen += "${request.method} ${request.url.encodedPath} ${buffer.readUtf8()}"
            val body = if (request.method == "GET") {
                """{"sessions":[{"id":"a","attention":{"section":"you","reason":"message","order_key":"k"},
                   "facts":{"agent":{"state":"working","since":"2026-09-30T19:00:00Z"},"jobs":{"text":"No jobs"},
                   "you":{"kind":"message","text":"Merge?","dismissible":true,"message_ids":["m1"]}}}]}"""
            } else {
                """{"facts":{"agent":{"state":"working"},"jobs":{"text":"No jobs"},"you":null,"finished":null}}"""
            }
            okhttp3.Response.Builder().request(request).protocol(okhttp3.Protocol.HTTP_1_1).code(200).message("OK")
                .body(body.toResponseBody("application/json".toMediaType())).build()
        }.build()
        val service = retrofit2.Retrofit.Builder().baseUrl("https://example.com/").client(client)
            .addConverterFactory(kotlinx.serialization.json.Json { ignoreUnknownKeys = true }.asConverterFactory("application/json".toMediaType()))
            .build().create(li.rajeshgo.sm.data.remote.ApiService::class.java)

        val session = service.getWatchState().sessions.single()
        assertEquals("you", session.attention?.section)
        assertEquals("Merge?", session.facts?.you?.text)
        assertEquals(listOf("m1"), session.facts?.you?.messageIds)
        val after = service.answerNeedsYou("a").facts
        assertNull(after?.you)
        assertEquals("working", after?.agent?.state)
        assertEquals(listOf("GET /watch/state ", "POST /sessions/a/needs-you/answered "), seen)
    }

    @Test
    fun aReadThatStartedBeforeSignOutDoesNotRefillTheCache() = kotlinx.coroutines.runBlocking {
        val board = li.rajeshgo.sm.data.model.BoardResponse(generatedAt = "old account")
        val returned = ScreenCache.remember({ ScreenCache.clear(); board }) { ScreenCache.board = it }
        assertEquals(board, returned)
        assertNull(ScreenCache.board)
        ScreenCache.remember({ board }) { ScreenCache.board = it }
        assertEquals(board, ScreenCache.board)
    }

    @Test
    fun signOutClearsEveryCachedTab() {
        ScreenCache.board = li.rajeshgo.sm.data.model.BoardResponse()
        ScreenCache.inbox["open"] = li.rajeshgo.sm.data.model.InboxResponse()
        ScreenCache.watch = emptyList()
        ScreenCache.clear()
        assertNull(ScreenCache.board)
        assertNull(ScreenCache.watch)
        assertEquals(0, ScreenCache.inbox.size)
    }
}
