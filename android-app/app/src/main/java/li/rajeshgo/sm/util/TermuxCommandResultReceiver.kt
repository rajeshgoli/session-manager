package li.rajeshgo.sm.util

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.withTimeoutOrNull
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

/** Explicit, one-shot result capability: other apps cannot address this receiver directly. */
class TermuxCommandResultReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val id = intent.data?.lastPathSegment ?: return
        val waiting = commands.remove(id) ?: return
        val result = intent.getBundleExtra("result")
        if (result == null) {
            waiting.completeExceptionally(IllegalStateException("Termux returned no command result"))
        } else if (result.getInt("err", -1) != -1 || result.getInt("exitCode", -1) != 0) {
            waiting.completeExceptionally(IllegalStateException(
                result.getString("errmsg")?.takeIf { it.isNotBlank() }
                    ?: result.getString("stderr")?.trim()?.takeIf { it.isNotBlank() }
                    ?: "Termux setup failed"))
        } else {
            waiting.complete(result.getString("stdout").orEmpty())
        }
    }

    companion object {
        private val commands = ConcurrentHashMap<String, CompletableDeferred<String>>()

        suspend fun run(context: Context, script: String): String {
            val id = UUID.randomUUID().toString()
            val waiting = CompletableDeferred<String>()
            commands[id] = waiting
            val callback = PendingIntent.getBroadcast(context, 0,
                Intent(context, TermuxCommandResultReceiver::class.java).setData(Uri.parse("sm-termux://result/$id")),
                PendingIntent.FLAG_ONE_SHOT or (if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0))
            try {
                val service = Intent("com.termux.RUN_COMMAND").apply {
                    setClassName("com.termux", "com.termux.app.RunCommandService")
                    putExtra("com.termux.RUN_COMMAND_PATH", "/data/data/com.termux/files/usr/bin/bash")
                    putExtra("com.termux.RUN_COMMAND_ARGUMENTS", arrayOf("-lc", script))
                    putExtra("com.termux.RUN_COMMAND_WORKDIR", "/data/data/com.termux/files/home")
                    putExtra("com.termux.RUN_COMMAND_BACKGROUND", true)
                    putExtra("com.termux.RUN_COMMAND_PENDING_INTENT", callback)
                }
                check(context.startService(service) != null) { "Install and open Termux first" }
                return withTimeoutOrNull(60_000) { waiting.await() }
                    ?: error("Termux did not respond. Enable allow-external-apps=true in ~/.termux/termux.properties, then retry.")
            } catch (error: SecurityException) {
                throw IllegalStateException("Allow Session Manager to run commands in Termux in Android Settings → Apps → Session Manager → Permissions.", error)
            } finally {
                commands.remove(id)
                callback.cancel()
            }
        }
    }
}
