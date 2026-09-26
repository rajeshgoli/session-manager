package li.rajeshgo.sm

import android.app.Application
import li.rajeshgo.sm.push.FollowPush

class SessionManagerApp : Application() {
    override fun onCreate() {
        super.onCreate()
        FollowPush.initialize(this)
    }
}
