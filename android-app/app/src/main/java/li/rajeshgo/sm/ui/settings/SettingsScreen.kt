package li.rajeshgo.sm.ui.settings

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
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
import li.rajeshgo.sm.auth.GoogleSignInManager
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Emerald
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
            text = "Native Android watch client for sm.rajeshgo.li with direct Termux attach.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
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
            text = "Attach note: install Termux, allow external apps, complete Cloudflare Access login there, and add your SSH public key once. Password auth is unsupported.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}
