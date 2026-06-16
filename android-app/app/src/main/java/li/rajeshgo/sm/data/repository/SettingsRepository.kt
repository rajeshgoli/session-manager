package li.rajeshgo.sm.data.repository

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.first
import li.rajeshgo.sm.data.security.CloudflareDeviceCredentialManager
import li.rajeshgo.sm.data.security.CloudflareDevicePrivateKeyProtector
import li.rajeshgo.sm.util.LocalDefaults
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map
import java.io.ByteArrayInputStream
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate

private val Context.dataStore: DataStore<Preferences> by preferencesDataStore(name = "session_manager_android")

class SettingsRepository(
    private val context: Context,
    private val cloudflarePrivateKeyProtector: CloudflareDevicePrivateKeyProtector = CloudflareDevicePrivateKeyProtector(),
) {
    private object Keys {
        val SERVER_URL = stringPreferencesKey("server_url")
        val ACCESS_TOKEN = stringPreferencesKey("access_token")
        val USER_EMAIL = stringPreferencesKey("user_email")
        val USER_NAME = stringPreferencesKey("user_name")
        val EXPIRES_AT = stringPreferencesKey("expires_at")
        val CLOUDFLARE_DEVICE_CERTIFICATE_ALIAS = stringPreferencesKey("cloudflare_device_certificate_alias")
        val CLOUDFLARE_DEVICE_CERTIFICATE_CHAIN_PEM = stringPreferencesKey("cloudflare_device_certificate_chain_pem")
        val LEGACY_CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8 = stringPreferencesKey("cloudflare_device_private_key_pkcs8")
        val CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8_WRAPPED =
            stringPreferencesKey("cloudflare_device_private_key_pkcs8_wrapped")
        val DISMISSED_UPDATE_ARTIFACT_HASH = stringPreferencesKey("dismissed_update_artifact_hash")
    }

    val serverUrl: Flow<String> = context.dataStore.data.map { prefs ->
        prefs[Keys.SERVER_URL] ?: LocalDefaults.defaultServerUrl
    }

    val accessToken: Flow<String> = context.dataStore.data.map { prefs ->
        prefs[Keys.ACCESS_TOKEN] ?: ""
    }

    val userEmail: Flow<String> = context.dataStore.data.map { prefs ->
        prefs[Keys.USER_EMAIL] ?: ""
    }

    val userName: Flow<String> = context.dataStore.data.map { prefs ->
        prefs[Keys.USER_NAME] ?: ""
    }

    val expiresAt: Flow<String> = context.dataStore.data.map { prefs ->
        prefs[Keys.EXPIRES_AT] ?: ""
    }

    val isLoggedIn: Flow<Boolean> = accessToken.map { it.isNotBlank() }

    val cloudflareDeviceCertificateAlias: Flow<String> = context.dataStore.data.map { prefs ->
        prefs[Keys.CLOUDFLARE_DEVICE_CERTIFICATE_ALIAS] ?: ""
    }

    val cloudflareDeviceCertificateChainPem: Flow<String> = context.dataStore.data.map { prefs ->
        prefs[Keys.CLOUDFLARE_DEVICE_CERTIFICATE_CHAIN_PEM] ?: ""
    }

    val hasCloudflareDeviceCertificate: Flow<Boolean> = context.dataStore.data.map { prefs ->
        val alias = prefs[Keys.CLOUDFLARE_DEVICE_CERTIFICATE_ALIAS]?.trim().orEmpty()
        val certificateChainPem = prefs[Keys.CLOUDFLARE_DEVICE_CERTIFICATE_CHAIN_PEM]?.trim().orEmpty()
        val privateKeyPkcs8 = cloudflareDevicePrivateKeyPkcs8FromPrefs(prefs)
        hasUsableCloudflareDeviceCredential(alias, certificateChainPem, privateKeyPkcs8)
    }

    val dismissedUpdateArtifactHash: Flow<String> = context.dataStore.data.map { prefs ->
        prefs[Keys.DISMISSED_UPDATE_ARTIFACT_HASH] ?: ""
    }

    suspend fun saveServerUrl(serverUrl: String) {
        context.dataStore.edit { prefs ->
            prefs[Keys.SERVER_URL] = serverUrl.trim().trimEnd('/')
        }
    }

    suspend fun saveAuth(token: String, email: String, name: String?, expiresAt: String) {
        context.dataStore.edit { prefs ->
            prefs[Keys.ACCESS_TOKEN] = token
            prefs[Keys.USER_EMAIL] = email
            prefs[Keys.USER_NAME] = name.orEmpty()
            prefs[Keys.EXPIRES_AT] = expiresAt
        }
    }

    suspend fun cloudflareDevicePrivateKeyPkcs8(): String {
        val prefs = context.dataStore.data.first()
        val privateKeyPkcs8 = cloudflareDevicePrivateKeyPkcs8FromPrefs(prefs)
        val legacyPrivateKeyPkcs8 = prefs[Keys.LEGACY_CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8]?.trim().orEmpty()
        if (privateKeyPkcs8.isNotBlank() && legacyPrivateKeyPkcs8.isNotBlank()) {
            context.dataStore.edit { updatedPrefs ->
                updatedPrefs[Keys.CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8_WRAPPED] =
                    cloudflarePrivateKeyProtector.protect(privateKeyPkcs8)
                updatedPrefs.remove(Keys.LEGACY_CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8)
            }
        }
        return privateKeyPkcs8
    }

    suspend fun saveCloudflareDeviceCredential(alias: String, certificateChainPem: String, privateKeyPkcs8: String) {
        context.dataStore.edit { prefs ->
            prefs[Keys.CLOUDFLARE_DEVICE_CERTIFICATE_ALIAS] = alias.trim()
            prefs[Keys.CLOUDFLARE_DEVICE_CERTIFICATE_CHAIN_PEM] = certificateChainPem.trim()
            prefs[Keys.CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8_WRAPPED] =
                cloudflarePrivateKeyProtector.protect(privateKeyPkcs8)
            prefs.remove(Keys.LEGACY_CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8)
        }
    }

    suspend fun clearCloudflareDeviceCredential() {
        context.dataStore.edit { prefs ->
            prefs.remove(Keys.CLOUDFLARE_DEVICE_CERTIFICATE_ALIAS)
            prefs.remove(Keys.CLOUDFLARE_DEVICE_CERTIFICATE_CHAIN_PEM)
            prefs.remove(Keys.CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8_WRAPPED)
            prefs.remove(Keys.LEGACY_CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8)
        }
    }

    suspend fun clearIncompleteCloudflareDeviceCredential(): String? {
        var removedAlias: String? = null
        context.dataStore.edit { prefs ->
            val alias = prefs[Keys.CLOUDFLARE_DEVICE_CERTIFICATE_ALIAS]?.trim().orEmpty()
            val certificateChainPem = prefs[Keys.CLOUDFLARE_DEVICE_CERTIFICATE_CHAIN_PEM]?.trim().orEmpty()
            val privateKey = storedCloudflarePrivateKey(prefs)
            if ((alias.isNotBlank() || certificateChainPem.isNotBlank()) && privateKey.isBlank()) {
                removedAlias = alias.takeIf { it.isNotBlank() }
                prefs.remove(Keys.CLOUDFLARE_DEVICE_CERTIFICATE_ALIAS)
                prefs.remove(Keys.CLOUDFLARE_DEVICE_CERTIFICATE_CHAIN_PEM)
            }
        }
        return removedAlias
    }

    suspend fun saveDismissedUpdateArtifactHash(artifactHash: String) {
        context.dataStore.edit { prefs ->
            prefs[Keys.DISMISSED_UPDATE_ARTIFACT_HASH] = artifactHash.trim().lowercase()
        }
    }

    suspend fun clearAuth() {
        context.dataStore.edit { prefs ->
            prefs.remove(Keys.ACCESS_TOKEN)
            prefs.remove(Keys.USER_EMAIL)
            prefs.remove(Keys.USER_NAME)
            prefs.remove(Keys.EXPIRES_AT)
        }
    }

    private fun hasUsableCloudflareDeviceCredential(
        alias: String,
        certificateChainPem: String,
        privateKeyPkcs8: String,
    ): Boolean =
        alias.isNotBlank() &&
            certificateChainPem.isNotBlank() &&
            privateKeyPkcs8.isNotBlank() &&
            cloudflareDeviceCertificateMatchesPrivateKey(certificateChainPem, privateKeyPkcs8)

    private fun cloudflareDeviceCertificateMatchesPrivateKey(
        certificateChainPem: String,
        privateKeyPkcs8: String,
    ): Boolean =
        runCatching {
            val certificates = CertificateFactory.getInstance("X.509")
                .generateCertificates(ByteArrayInputStream(certificateChainPem.toByteArray()))
                .filterIsInstance<X509Certificate>()
            val leaf = certificates.firstOrNull()
            leaf != null &&
                leaf.publicKey.algorithm.equals("RSA", ignoreCase = true) &&
                CloudflareDeviceCredentialManager().certificateChainMatchesPrivateKey(certificateChainPem, privateKeyPkcs8)
        }.getOrDefault(false)

    private fun cloudflareDevicePrivateKeyPkcs8FromPrefs(prefs: Preferences): String {
        val storedPrivateKey = storedCloudflarePrivateKey(prefs)
        return cloudflarePrivateKeyProtector.unprotect(storedPrivateKey).orEmpty()
    }

    private fun storedCloudflarePrivateKey(prefs: Preferences): String =
        prefs[Keys.CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8_WRAPPED]?.trim().orEmpty()
            .ifBlank {
                prefs[Keys.LEGACY_CLOUDFLARE_DEVICE_PRIVATE_KEY_PKCS8]?.trim().orEmpty()
            }
}
