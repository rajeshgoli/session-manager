package li.rajeshgo.sm.data.remote

import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** Spec 2170 A.5 (sm#2176): the headers every app request carries, through the real interceptor. */
class ClientHeadersTest {
    private var clock = 10_000L
    private val nav = ClientNavState { clock }
    private var sent: Request? = null
    private val client = OkHttpClient.Builder()
        .addInterceptor(AuthInterceptor({ "token" }, nav))
        .addInterceptor { chain ->
            sent = chain.request()
            Response.Builder().request(chain.request()).protocol(Protocol.HTTP_1_1).code(200).message("OK")
                .header("x-sm-switches", "42").body("{}".toResponseBody()).build()
        }
        .build()

    private fun get(bg: Boolean = false): Request {
        val request = Request.Builder().url("http://sm.test/client/board")
        if (bg) request.header("x-sm-bg", "1")
        client.newCall(request.build()).execute().close()
        return sent!!
    }

    @Test
    fun aColdStartsFirstOwnerRequestSaysLaunchAndLaterOnesSayNav() {
        nav.arrived("launch")
        nav.setRoute("board")
        val first = get()
        assertEquals("Bearer token", first.header("Authorization"))
        assertEquals("board", first.header("x-sm-screen"))
        assertEquals("launch", first.header("x-sm-nav"))
        assertNull(first.header("x-sm-bg"))
        assertEquals("nav", get().header("x-sm-nav"))
        assertEquals(42L, nav.switchVersion.value)
    }

    @Test
    fun aRequestLongAfterTheLastTapIsBackground() {
        nav.setRoute("inbox")
        clock += ClientNavState.FOREGROUND_MS + 1
        val poll = get()
        assertEquals("1", poll.header("x-sm-bg"))
        assertNull(poll.header("x-sm-nav"))
        assertEquals("inbox", poll.header("x-sm-screen"))
        nav.acted()
        assertEquals("nav", get().header("x-sm-nav"))
    }

    @Test
    fun aNotificationTapSendsItsNoticeIdOnceAndAMarkedRefreshDoesNotSpendIt() {
        nav.setRoute("inbox")
        nav.push("inbox/thread")
        nav.arrived(ClientNavState.pushNav("n_442"))
        val refresh = get(bg = true)
        assertEquals("1", refresh.header("x-sm-bg"))
        assertNull(refresh.header("x-sm-nav"))
        val tap = get()
        assertEquals("push:n_442", tap.header("x-sm-nav"))
        assertEquals("inbox/thread", tap.header("x-sm-screen"))
        nav.pop("inbox/thread")
        assertEquals("inbox", get().header("x-sm-screen"))
        assertEquals("push", ClientNavState.pushNav(null))
    }

    @Test
    fun anSmLinkFromOutsideTheAppSaysLink() {
        nav.arrived("link")
        assertEquals("link", get().header("x-sm-nav"))
    }
}
