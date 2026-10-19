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
import li.rajeshgo.sm.data.model.ClientSession

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
fun ShellWatchCard(session: ClientSession, expanded: Boolean, toggle: () -> Unit, open: () -> Unit, retire: () -> Unit, depth: Int) {
    Surface(Modifier.padding(start = (depth * 14).dp), shape = MaterialTheme.shapes.medium, tonalElevation = 1.dp) {
        Column(Modifier.fillMaxWidth().padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(sessionDisplayName(session), style = MaterialTheme.typography.titleSmall, modifier = Modifier.fillMaxWidth().clickable(onClick = toggle))
            Text(rowFactsLine(session), style = MaterialTheme.typography.bodySmall)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (!isStoppedSession(session)) {
                    TextButton(onClick = open) { Text("Open in Termux") }
                    TextButton(onClick = retire) { Text("Retire") }
                }
            }
            if (expanded) Text(session.workingDir, style = MaterialTheme.typography.bodySmall)
        }
    }
}
