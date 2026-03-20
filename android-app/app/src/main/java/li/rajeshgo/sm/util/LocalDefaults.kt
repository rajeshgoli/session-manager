package li.rajeshgo.sm.util

import li.rajeshgo.sm.BuildConfig

object LocalDefaults {
    val defaultServerUrl: String = BuildConfig.SM_DEFAULT_SERVER_URL.trim().trimEnd('/')
    val googleServerClientId: String = BuildConfig.SM_GOOGLE_SERVER_CLIENT_ID.trim()
}
