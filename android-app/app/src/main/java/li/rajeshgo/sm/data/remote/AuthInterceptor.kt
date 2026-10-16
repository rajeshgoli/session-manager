package li.rajeshgo.sm.data.remote

import android.os.SystemClock
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import okhttp3.Interceptor
import okhttp3.Response

class AuthInterceptor(
    private val getToken: () -> String,
    private val nav: ClientNavState = ClientNav,
) : Interceptor {
    override fun intercept(chain: Interceptor.Chain): Response {
        val token = getToken().trim()
        val builder = chain.request().newBuilder()
        if (token.isNotBlank()) builder.header("Authorization", "Bearer $token")
        // A request the app marks background (a refresh it makes itself) never counts as the owner's.
        val background = chain.request().header("x-sm-bg") == "1"
        nav.headers(background).forEach { (name, value) -> builder.header(name, value) }
        val response = chain.proceed(builder.build())
        response.header(ClientNavState.SWITCHES_HEADER)?.toLongOrNull()?.let(nav::noteSwitchVersion)
        return response
    }
}

/**
 * The client headers of spec 2170 A.5 (sm#2176): `x-sm-screen` names the
 * screen showing; a request within [FOREGROUND_MS] of a tap, key or screen
 * change is the owner's and carries `x-sm-nav`; anything later is a refresh
 * and carries `x-sm-bg: 1`. The first owner request after a cold start says
 * `launch`, after a notification tap `push:<notice id>`, after an sm link
 * opened from outside the app `link`; the rest say `nav`.
 */
open class ClientNavState(private val now: () -> Long) {
    private val lock = Any()
    private val screens = ArrayList<String>()
    private var route: String? = null
    private var pending: String? = null
    private var lastActed = Long.MIN_VALUE / 2
    private val version = MutableStateFlow<Long?>(null)

    /** The version every response names (`x-sm-switches`); a change re-reads the switches (B.1). */
    val switchVersion: StateFlow<Long?> = version

    fun acted() = synchronized(lock) { lastActed = now() }

    /** How the app was opened, sent with the next owner request. */
    fun arrived(how: String) = synchronized(lock) {
        pending = how
        lastActed = now()
    }

    /** The bottom-level screen, from the navigation route. */
    fun setRoute(name: String?) = synchronized(lock) {
        if (name != route) lastActed = now()
        route = name
    }

    /** A screen shown over the route: a thread, a doc, an agent's detail. */
    fun push(name: String) = synchronized(lock) {
        screens.add(name)
        lastActed = now()
    }

    fun pop(name: String) = synchronized(lock) {
        val at = screens.lastIndexOf(name)
        if (at >= 0) screens.removeAt(at)
    }

    val screen: String? get() = synchronized(lock) { screens.lastOrNull() ?: route }

    fun headers(background: Boolean = false): Map<String, String> = synchronized(lock) {
        val out = linkedMapOf<String, String>()
        (screens.lastOrNull() ?: route)?.let { out["x-sm-screen"] = it }
        if (background || now() - lastActed > FOREGROUND_MS) {
            out["x-sm-bg"] = "1"
        } else {
            out["x-sm-nav"] = pending ?: "nav"
            pending = null
        }
        out
    }

    fun noteSwitchVersion(value: Long) {
        version.value = value
    }

    companion object {
        const val FOREGROUND_MS = 2_000L
        const val SWITCHES_HEADER = "x-sm-switches"

        /** `push:<id>` for a tapped notification, as the server reads it. */
        fun pushNav(noticeId: String?): String =
            noticeId?.trim()?.takeIf { it.isNotEmpty() }?.let { "push:$it" } ?: "push"
    }
}

object ClientNav : ClientNavState({ SystemClock.elapsedRealtime() })
