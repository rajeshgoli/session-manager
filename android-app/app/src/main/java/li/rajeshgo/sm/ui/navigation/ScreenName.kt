package li.rajeshgo.sm.ui.navigation

import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import li.rajeshgo.sm.data.remote.ClientNav

/** Names the screen shown over the current route while this is composed (spec 2170 A.5 `x-sm-screen`). */
@Composable
fun ScreenName(name: String) {
    DisposableEffect(name) {
        ClientNav.push(name)
        onDispose { ClientNav.pop(name) }
    }
}

/** The `x-sm-screen` name of a navigation route: the Agents tab is `agents`, a route's arguments are dropped. */
fun screenForRoute(route: String?): String? = when (val base = route?.substringBefore('?')) {
    null -> null
    Routes.WATCH -> "agents"
    Routes.AUTOPM -> "settings/autopm"
    else -> base
}
