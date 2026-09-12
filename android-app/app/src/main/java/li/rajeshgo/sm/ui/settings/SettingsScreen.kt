package li.rajeshgo.sm.ui.settings

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.rounded.ArrowBack
import androidx.compose.material.icons.rounded.ExpandMore
import androidx.compose.material.icons.rounded.ExpandLess
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import li.rajeshgo.sm.BuildConfig
import li.rajeshgo.sm.auth.GoogleSignInManager
import li.rajeshgo.sm.ui.theme.*
import li.rajeshgo.sm.util.LocalDefaults
import kotlinx.coroutines.launch

@Composable
fun SettingsScreen(
    onNavigateToWatch: () -> Unit,
    pendingEnrollmentUrl: String? = null,
    onEnrollmentDeepLinkConsumed: () -> Unit = {},
    viewModel: SettingsViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    LaunchedEffect(Unit) { while (true) { viewModel.refreshStudioSshStatus(); kotlinx.coroutines.delay(10_000) } }
    val context = LocalContext.current
    val signIn = remember(context) { GoogleSignInManager(context) }
    val scope = rememberCoroutineScope()
    var advanced by rememberSaveable { mutableStateOf(false) }
    val clientId = state.bootstrap?.auth?.googleServerClientId?.takeIf { it.isNotBlank() } ?: LocalDefaults.googleServerClientId
    Column(
        Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding().imePadding()
            .verticalScroll(rememberScrollState()).padding(horizontal = 20.dp).padding(bottom = 24.dp),
        verticalArrangement = Arrangement.spacedBy(20.dp),
    ) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = onNavigateToWatch) { Icon(Icons.AutoMirrored.Rounded.ArrowBack, "Back") }
            Text("Settings", style = MaterialTheme.typography.headlineSmall)
        }
        SettingsGroup("Account") {
            if (state.isLoggedIn) {
                Text(state.userName.ifBlank { state.userEmail }, style = MaterialTheme.typography.titleMedium)
                if (state.userName.isNotBlank() && state.userName != state.userEmail) Text(state.userEmail, style = MaterialTheme.typography.bodyMedium, color = TextMuted)
                TextButton(onClick = { scope.launch { signIn.clearCredentialState(); viewModel.finishLogout() } }) { Text("Sign out") }
            } else {
                Text("Sign in to manage your agents.", color = TextSecondary)
                Button(onClick = {
                    viewModel.refreshBootstrap()
                    scope.launch {
                        signIn.getIdToken(clientId).onSuccess { viewModel.exchangeGoogleIdToken(it.idToken, onNavigateToWatch) }
                            .onFailure { viewModel.reportError(it.message ?: "Couldn't sign in. Try again.") }
                    }
                }, enabled = !state.loading && state.serverUrl.isNotBlank()) { Text(if (state.loading) "Signing in…" else "Sign in with Google") }
            }
            state.error?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = Rose) }
        }
        ConnectionSettings(state, viewModel, pendingEnrollmentUrl, onEnrollmentDeepLinkConsumed)
        SettingsGroup("App updates") {
            Text("Version ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE})", style = MaterialTheme.typography.bodyMedium, color = TextSecondary)
            val update = state.availableUpdate
            if (update == null) {
                TextButton(onClick = viewModel::refreshUpdate) { Text("Check for updates") }
            } else {
                Text("${update.versionName} is available", color = Emerald)
                Button(onClick = viewModel::installUpdate, enabled = !state.updateInstalling) { Text(if (state.updateInstalling) "Downloading…" else "Install update") }
            }
            state.updateError?.let { Text(it, color = Rose, style = MaterialTheme.typography.bodySmall) }
        }
        Row(Modifier.fillMaxWidth().clickable { advanced = !advanced }.padding(vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            Text("Advanced", Modifier.weight(1f), color = TextMuted, style = MaterialTheme.typography.titleSmall)
            Icon(if (advanced) Icons.Rounded.ExpandLess else Icons.Rounded.ExpandMore, "Advanced settings", tint = TextMuted)
        }
        if (advanced) AdvancedSettings(state, viewModel)
    }
}

@Composable
private fun SettingsGroup(title: String, content: @Composable ColumnScope.() -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(title, style = MaterialTheme.typography.labelLarge, color = TextMuted, modifier = Modifier.padding(start = 4.dp))
        Surface(shape = RoundedCornerShape(18.dp), color = PanelElevated) {
            Column(Modifier.fillMaxWidth().padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp), content = content)
        }
    }
}

@Composable
private fun ConnectionSettings(state: SettingsUiState, viewModel: SettingsViewModel, pendingUrl: String?, onConsumed: () -> Unit) {
    SettingsGroup("Connections") {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text("Studio SSH", style = MaterialTheme.typography.titleMedium)
                Text(if (state.studioSshBusy) "Updating…" else if (!state.studioSshLoaded) "Checking…" else if (state.studioSshStatus == "starting") "Starting…" else if (state.studioSshEnabled) "On" else "Off", color = TextMuted)
            }
            Switch(checked = state.studioSshEnabled, onCheckedChange = viewModel::toggleStudioSsh, enabled = state.isLoggedIn && !state.studioSshBusy && state.studioSshLoaded)
        }
        if (state.studioSshEnabled && state.studioSshHost.isNotBlank()) Text(state.studioSshHost, style = MaterialTheme.typography.bodySmall, color = TextMuted)
        state.studioSshError?.let { Text(it, color = Rose, style = MaterialTheme.typography.bodySmall); TextButton(onClick = viewModel::refreshStudioSshStatus) { Text("Try again") } }
        HorizontalDivider(color = Border)
        Text("This device", style = MaterialTheme.typography.titleMedium)
        Text(if (state.cloudflareDeviceCertificateConfigured) "Registered" else "Not registered", color = if (state.cloudflareDeviceCertificateConfigured) Emerald else TextMuted)
        var hostOpen by remember { mutableStateOf(false) }
    if (hostOpen) HostStatusSheet(state, viewModel::refreshHostStatus) { hostOpen = false }
    TextButton(onClick = { hostOpen = true }) { Text("Host status") }
    val enrollmentUrl = pendingUrl?.trim()?.takeIf { it.isNotBlank() }
        if (enrollmentUrl != null) {
            Text("A device registration link is ready.")
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = { viewModel.enrollCloudflareDeviceFromQr(enrollmentUrl); onConsumed() }, enabled = !state.cloudflareEnrollmentInProgress) { Text("Register device") }
                TextButton(onClick = onConsumed) { Text("Dismiss") }
            }
        } else if (!state.cloudflareDeviceCertificateConfigured) {
            Text("Scan the registration QR code from your Session Manager host.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
        }
        if (state.cloudflareEnrollmentInProgress) LinearProgressIndicator(Modifier.fillMaxWidth())
        state.cloudflareEnrollmentError?.let { Text(it, color = Rose, style = MaterialTheme.typography.bodySmall) }
    }
}

@Composable
private fun AdvancedSettings(state: SettingsUiState, viewModel: SettingsViewModel) {
    val context = LocalContext.current
    var deviceTools by rememberSaveable { mutableStateOf(false) }
    var confirmRemoveDevice by remember { mutableStateOf(false) }
    SettingsGroup("Server") {
        OutlinedTextField(state.serverUrl, viewModel::updateServerUrl, label = { Text("Server address") }, singleLine = true, modifier = Modifier.fillMaxWidth())
        TextButton(onClick = { viewModel.refreshBootstrap(); viewModel.refreshStudioSshStatus() }) { Text("Save connection") }
    }
    SettingsGroup("Device registration") {
        TextButton(onClick = { deviceTools = !deviceTools }) { Text(if (deviceTools) "Hide registration details" else "Show registration details") }
        if (deviceTools) {
            Text(state.mobileDeviceKeyId, style = MaterialTheme.typography.bodySmall, color = TextMuted)
            Text("For manual setup on your Session Manager host.", style = MaterialTheme.typography.bodySmall, color = TextMuted)
            TextButton(onClick = {
                val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                val config = "id: ${state.mobileDeviceKeyId}\npublic_key: |\n" + state.mobileDevicePublicKey.lines().joinToString("\n") { "  $it" }
                clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("Device registration", config))
            }, enabled = state.mobileDevicePublicKey.isNotBlank()) { Text("Copy registration details") }
            state.mobileDeviceKeyError?.let { Text(it, color = Rose); TextButton(onClick = viewModel::loadMobileDeviceKey) { Text("Retry loading device key") } }
        }
        TextButton(onClick = { confirmRemoveDevice = true }, enabled = state.cloudflareDeviceCertificateConfigured && !state.cloudflareEnrollmentInProgress) { Text("Remove device registration", color = Rose) }
    }
    if (confirmRemoveDevice) AlertDialog(
        onDismissRequest = { confirmRemoveDevice = false }, title = { Text("Remove registration?") },
        text = { Text("You’ll need to register this device again to connect remotely.") },
        confirmButton = { TextButton(onClick = { confirmRemoveDevice = false; viewModel.clearCloudflareDeviceCertificateChain() }) { Text("Remove") } },
        dismissButton = { TextButton(onClick = { confirmRemoveDevice = false }) { Text("Cancel") } },
    )
}
