package li.rajeshgo.sm.util

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.widget.Toast
import li.rajeshgo.sm.data.model.TermuxAttachMetadata

private const val TERMUX_PACKAGE = "com.termux"
private const val TERMUX_RUN_COMMAND_ACTION = "com.termux.RUN_COMMAND"
private const val TERMUX_RUN_COMMAND_SERVICE = "com.termux.app.RunCommandService"
private const val TERMUX_RUN_COMMAND_PATH = "com.termux.RUN_COMMAND_PATH"
private const val TERMUX_RUN_COMMAND_ARGUMENTS = "com.termux.RUN_COMMAND_ARGUMENTS"
private const val TERMUX_RUN_COMMAND_WORKDIR = "com.termux.RUN_COMMAND_WORKDIR"
private const val TERMUX_RUN_COMMAND_BACKGROUND = "com.termux.RUN_COMMAND_BACKGROUND"
private const val TERMUX_BASH_PATH = "/data/data/com.termux/files/usr/bin/bash"
private const val TERMUX_HOME = "/data/data/com.termux/files/home"

fun termuxAttachCommand(attach: TermuxAttachMetadata): String? {
    if (!attach.supported) {
        return null
    }
    if (!attach.sshCommand.isNullOrBlank()) {
        return attach.sshCommand
    }
    if (attach.sshHost.isNullOrBlank() || attach.sshUsername.isNullOrBlank() || attach.tmuxSession.isNullOrBlank()) {
        return null
    }
    val sessionName = attach.tmuxSession.replace("'", "'\"'\"'")
    return "ssh -t ${attach.sshUsername}@${attach.sshHost} 'tmux attach-session -t $sessionName'"
}

fun launchTermuxAttach(context: Context, attach: TermuxAttachMetadata): Result<Unit> {
    val command = termuxAttachCommand(attach)
        ?: return Result.failure(IllegalStateException(attach.reason ?: "Attach unavailable"))

    return try {
        val intent = Intent(TERMUX_RUN_COMMAND_ACTION).apply {
            setClassName(TERMUX_PACKAGE, TERMUX_RUN_COMMAND_SERVICE)
            putExtra(TERMUX_RUN_COMMAND_PATH, TERMUX_BASH_PATH)
            putExtra(TERMUX_RUN_COMMAND_ARGUMENTS, arrayOf("-lc", command))
            putExtra(TERMUX_RUN_COMMAND_WORKDIR, TERMUX_HOME)
            putExtra(TERMUX_RUN_COMMAND_BACKGROUND, false)
        }
        context.startService(intent)
        Result.success(Unit)
    } catch (error: Exception) {
        val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
        clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm attach", command))
        context.packageManager.getLaunchIntentForPackage(TERMUX_PACKAGE)?.let { launchIntent ->
            launchIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            context.startActivity(launchIntent)
            Toast.makeText(context, "Attach command copied for Termux.", Toast.LENGTH_SHORT).show()
            return Result.success(Unit)
        }
        Result.failure(
            when (error) {
                is ActivityNotFoundException -> IllegalStateException("Termux is not installed")
                else -> error
            }
        )
    }
}
