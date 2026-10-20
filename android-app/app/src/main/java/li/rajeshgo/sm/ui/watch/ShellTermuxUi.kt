package li.rajeshgo.sm.ui.watch

import android.content.pm.PackageManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.graphics.Color
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextMuted
import li.rajeshgo.sm.ui.theme.TextSecondary

@Composable
fun ShellTermuxDialogs(state: WatchUiState, setup: () -> Unit, dismiss: () -> Unit, dismissError: () -> Unit) {
    val context = LocalContext.current
    var permissionDenied by remember { mutableStateOf(false) }
    val permission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) setup() else permissionDenied = true
    }
    if (state.shellSetupId != null) AlertDialog(
        onDismissRequest = { if (state.shellAttachStatus == null) dismiss() },
        title = { Text("Set up Termux for shells") },
        text = { Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text("Open Termux once and run: pkg install openssh cloudflared\n\nSet allow-external-apps=true in ~/.termux/termux.properties.")
            Text("Setup keeps your SSH private key in Termux, registers its public key with this phone, and adds studio-lan and studio-away to your SSH config.")
            if (permissionDenied) Text("Allow Run commands in Termux in Android Settings → Apps → Session Manager → Permissions.", color = MaterialTheme.colorScheme.error)
            state.shellAttachStatus?.let { Text(it) }
            state.shellAttachError?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        } },
        confirmButton = { TextButton(enabled = state.shellAttachStatus == null, onClick = {
            if (context.checkSelfPermission("com.termux.permission.RUN_COMMAND") == PackageManager.PERMISSION_GRANTED) setup()
            else permission.launch("com.termux.permission.RUN_COMMAND")
        }) { Text("Set up Termux") } },
        dismissButton = { TextButton(enabled = state.shellAttachStatus == null, onClick = dismiss) { Text("Cancel") } },
    ) else if (state.shellAttachStatus != null || state.shellAttachError != null) AlertDialog(
        onDismissRequest = { if (state.shellAttachStatus == null) dismissError() },
        title = { Text(if (state.shellAttachError == null) "Open in Termux" else "Could not open Termux") },
        text = { Text(state.shellAttachError ?: state.shellAttachStatus.orEmpty()) },
        confirmButton = { if (state.shellAttachStatus == null) TextButton(onClick = dismissError) { Text("Close") } },
    )
}

@Composable
fun ShellWatchCard(
    session: ClientSession, expanded: Boolean, toggle: () -> Unit, open: () -> Unit, retire: () -> Unit, depth: Int,
    update: (name: String?, hidden: Boolean?) -> Unit,
) {
    var renaming by remember(session.id) { mutableStateOf(false) }
    if (renaming) ShellRenameDialog(session, onDismiss = { renaming = false }) { name -> renaming = false; update(name, null) }
    Surface(Modifier.padding(start = (depth * 14).dp), shape = MaterialTheme.shapes.medium, tonalElevation = if (session.hidden) 0.dp else 1.dp) {
        Column(Modifier.fillMaxWidth().padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(Modifier.fillMaxWidth().clickable(onClick = toggle), verticalAlignment = androidx.compose.ui.Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(sessionDisplayName(session), style = MaterialTheme.typography.titleSmall, color = if (session.hidden) TextSecondary else Color.Unspecified, modifier = Modifier.weight(1f, fill = false))
                if (session.hidden) Text("Hidden", style = MaterialTheme.typography.labelSmall, color = TextMuted)
            }
            Text(rowFactsLine(session), style = MaterialTheme.typography.bodySmall)
            if (!isStoppedSession(session)) Row(Modifier.fillMaxWidth(), verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
                TextButton(onClick = open) { Text("Open in Termux") }
                TextButton(onClick = { renaming = true }) { Text("Rename") }
                TextButton(onClick = { update(null, !session.hidden) }) { Text(if (session.hidden) "Unhide" else "Hide") }
                Spacer(Modifier.weight(1f))
                // Closing ends the shell, so it reads like Retire.
                TextButton(onClick = retire) { Text("Close", color = Rose) }
            }
            if (expanded) Text(session.workingDir, style = MaterialTheme.typography.bodySmall)
        }
    }
}

/** The server allows this many characters in a shell name. */
private const val SHELL_NAME_MAX = 40

/** Blank restores the default `zsh · folder` name. */
@Composable
private fun ShellRenameDialog(session: ClientSession, onDismiss: () -> Unit, save: (String) -> Unit) {
    var text by remember(session.id) { mutableStateOf(session.name) }
    val folder = session.workingDir.trimEnd('/').substringAfterLast('/')
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Rename shell") },
        text = {
            OutlinedTextField(
                value = text, onValueChange = { text = it.replace("\n", "").take(SHELL_NAME_MAX) }, singleLine = true,
                placeholder = { Text("zsh · $folder") },
                supportingText = { Text("Leave blank for zsh · $folder") },
            )
        },
        confirmButton = { TextButton(onClick = { save(text) }, enabled = text.trim() != session.name) { Text("Rename") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
