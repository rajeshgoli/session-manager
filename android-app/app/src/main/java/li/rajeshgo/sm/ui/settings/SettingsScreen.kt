package li.rajeshgo.sm.ui.settings

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.lifecycle.viewmodel.compose.viewModel
import li.rajeshgo.sm.BuildConfig
import li.rajeshgo.sm.auth.GoogleSignInManager
import li.rajeshgo.sm.ui.theme.BorderStrong
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Emerald
import li.rajeshgo.sm.ui.theme.PanelElevated
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.util.LocalDefaults
import kotlinx.coroutines.launch

@Composable
fun SettingsScreen(
    onNavigateToWatch: () -> Unit,
    viewModel: SettingsViewModel = viewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val context = LocalContext.current
    val googleSignInManager = GoogleSignInManager(context)
    val coroutineScope = rememberCoroutineScope()
    val effectiveGoogleClientId = state.bootstrap?.auth?.googleServerClientId?.takeIf { it.isNotBlank() }
        ?: LocalDefaults.googleServerClientId.takeIf { it.isNotBlank() }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(24.dp),
        verticalArrangement = Arrangement.Center,
    ) {
        Text(
            text = "Session Manager",
            style = MaterialTheme.typography.headlineMedium,
            color = MaterialTheme.colorScheme.onBackground,
        )
        Spacer(Modifier.height(8.dp))
        Text(
            text = "Native Android watch client for sm.rajeshgo.li with in-app HTTPS terminal attach.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(8.dp))
        Text(
            text = "App version ${BuildConfig.VERSION_NAME}",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            fontFamily = FontFamily.Monospace,
        )

        Spacer(Modifier.height(24.dp))

        OutlinedTextField(
            value = state.serverUrl,
            onValueChange = viewModel::updateServerUrl,
            label = { Text("Server URL") },
            placeholder = { Text(LocalDefaults.defaultServerUrl.ifBlank { "https://your-sm-host" }) },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )

        Spacer(Modifier.height(16.dp))

        Text(
            text = "Google server client ID",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(4.dp))
        Text(
            text = when {
                !state.bootstrap?.auth?.googleServerClientId.isNullOrBlank() -> "Loaded from server bootstrap"
                !LocalDefaults.googleServerClientId.isBlank() -> "Configured in local.defaults.properties"
                else -> "Not configured"
            },
            style = MaterialTheme.typography.bodySmall,
            color = if (effectiveGoogleClientId.isNullOrBlank()) Rose else Emerald,
            fontFamily = FontFamily.Monospace,
        )

        state.bootstrap?.externalAccess?.publicSshHost?.let { host ->
            Spacer(Modifier.height(16.dp))
            Text(
                text = "Remote attach host",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(4.dp))
            Text(
                text = host,
                style = MaterialTheme.typography.bodySmall,
                color = Cyan,
                fontFamily = FontFamily.Monospace,
            )
        }

        Spacer(Modifier.height(24.dp))

        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = PanelElevated),
            border = androidx.compose.foundation.BorderStroke(1.dp, BorderStrong),
        ) {
            Column(modifier = Modifier.padding(16.dp)) {
                Text(
                    text = "Mobile HTTPS attach",
                    style = MaterialTheme.typography.titleMedium,
                    color = MaterialTheme.colorScheme.onSurface,
                )
                Spacer(Modifier.height(8.dp))
                Text(
                    text = "Register this public key under mobile_terminal.allowed_users in Session Manager config to enable in-app terminal attach.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(10.dp))
                Text(
                    text = "Device key id",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Text(
                    text = state.mobileDeviceKeyId.ifBlank { "unavailable" },
                    style = MaterialTheme.typography.bodySmall,
                    color = if (state.mobileDeviceKeyId.isBlank()) Rose else Cyan,
                    fontFamily = FontFamily.Monospace,
                )
                Spacer(Modifier.height(8.dp))
                Text(
                    text = state.mobileDevicePublicKey.ifBlank { state.mobileDeviceKeyError ?: "No key generated yet" },
                    style = MaterialTheme.typography.bodySmall,
                    color = if (state.mobileDeviceKeyError == null) MaterialTheme.colorScheme.onSurfaceVariant else Rose,
                    fontFamily = FontFamily.Monospace,
                )
                Spacer(Modifier.height(10.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(onClick = viewModel::loadMobileDeviceKey) {
                        Text("Refresh key")
                    }
                    Button(
                        onClick = {
                            val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                            val text = "id: ${state.mobileDeviceKeyId}\npublic_key: |\n" +
                                state.mobileDevicePublicKey.lines().joinToString("\n") { "  $it" }
                            clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm mobile attach key", text))
                        },
                        enabled = state.mobileDeviceKeyId.isNotBlank() && state.mobileDevicePublicKey.isNotBlank(),
                    ) {
                        Text("Copy config")
                    }
                }
            }
        }

        Spacer(Modifier.height(24.dp))

        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = PanelElevated),
            border = androidx.compose.foundation.BorderStroke(1.dp, BorderStrong),
        ) {
            Column(modifier = Modifier.padding(16.dp)) {
                Text(
                    text = "App updates",
                    style = MaterialTheme.typography.titleMedium,
                    color = MaterialTheme.colorScheme.onSurface,
                )
                Spacer(Modifier.height(8.dp))
                val availableUpdate = state.availableUpdate
                if (availableUpdate == null) {
                    Text(
                        text = "This build matches the latest published artifact.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(12.dp))
                    OutlinedButton(onClick = viewModel::refreshUpdate, modifier = Modifier.fillMaxWidth()) {
                        Text("Check for update")
                    }
                } else {
                    Text(
                        text = "Update available: ${availableUpdate.versionName}",
                        style = MaterialTheme.typography.bodyMedium,
                        color = Emerald,
                    )
                    availableUpdate.uploadedAt?.let { uploadedAt ->
                        Spacer(Modifier.height(4.dp))
                        Text(
                            text = "Published $uploadedAt",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    Spacer(Modifier.height(12.dp))
                    Button(
                        onClick = viewModel::installUpdate,
                        enabled = !state.updateInstalling,
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        if (state.updateInstalling) {
                            CircularProgressIndicator(strokeWidth = 2.dp, modifier = Modifier.height(18.dp))
                        } else {
                            Text("Install update")
                        }
                    }
                    Spacer(Modifier.height(8.dp))
                    OutlinedButton(
                        onClick = viewModel::dismissUpdate,
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        Text("Dismiss this version")
                    }
                }

                state.updateError?.let { updateError ->
                    Spacer(Modifier.height(12.dp))
                    Text(
                        text = updateError,
                        style = MaterialTheme.typography.bodySmall,
                        color = Rose,
                    )
                }
            }
        }

        Spacer(Modifier.height(24.dp))

        if (state.isLoggedIn) {
            Text(
                text = "Signed in as ${state.userName.ifBlank { state.userEmail }}",
                style = MaterialTheme.typography.bodyMedium,
                color = Emerald,
            )
            Spacer(Modifier.height(16.dp))
            Button(onClick = onNavigateToWatch, modifier = Modifier.fillMaxWidth()) {
                Text("Open Watch")
            }
            Spacer(Modifier.height(12.dp))
            OutlinedButton(
                onClick = {
                    coroutineScope.launch {
                        googleSignInManager.clearCredentialState()
                        viewModel.finishLogout()
                    }
                },
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text("Sign Out")
            }
        } else {
            Button(
                onClick = {
                    viewModel.refreshBootstrap()
                    coroutineScope.launch {
                        googleSignInManager.getIdToken(effectiveGoogleClientId.orEmpty())
                            .onSuccess { credential ->
                                viewModel.exchangeGoogleIdToken(credential.idToken, onNavigateToWatch)
                            }
                            .onFailure { error ->
                                viewModel.reportError(error.message ?: "Google sign-in failed")
                            }
                    }
                },
                enabled = !state.loading && state.serverUrl.isNotBlank(),
                modifier = Modifier.fillMaxWidth(),
            ) {
                if (state.loading) {
                    CircularProgressIndicator(strokeWidth = 2.dp, modifier = Modifier.height(18.dp))
                } else {
                    Text("Sign in with Google")
                }
            }
        }

        state.error?.let { error ->
            Spacer(Modifier.height(16.dp))
            Text(
                text = error,
                style = MaterialTheme.typography.bodySmall,
                color = Rose,
            )
        }

        Spacer(Modifier.height(24.dp))
        Text(
            text = "Attach note: HTTPS in-app attach is primary when mobile_terminal is enabled server-side. Termux remains a temporary fallback for sessions without mobile terminal support.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}
