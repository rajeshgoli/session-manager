package li.rajeshgo.sm.data.remote

import android.util.Log
import kotlinx.coroutines.flow.first
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.data.security.DeviceKeyManager
import okhttp3.OkHttpClient
import okhttp3.logging.HttpLoggingInterceptor
import java.io.ByteArrayInputStream
import java.net.Socket
import java.security.KeyStore
import java.security.Principal
import java.security.PrivateKey
import java.security.SecureRandom
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import java.util.concurrent.TimeUnit
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLEngine
import javax.net.ssl.SSLSocketFactory
import javax.net.ssl.TrustManagerFactory
import javax.net.ssl.X509ExtendedKeyManager
import javax.net.ssl.X509TrustManager

class HttpClientFactory(
    private val settingsRepository: SettingsRepository? = null,
    private val deviceKeyManager: DeviceKeyManager? = null,
) {
    suspend fun create(
        token: String = "",
        includeLogging: Boolean = true,
        connectTimeoutSeconds: Long = 10,
        readTimeoutSeconds: Long = 30,
    ): OkHttpClient {
        val builder = OkHttpClient.Builder()
            .followRedirects(false)
            .followSslRedirects(false)
            .connectTimeout(connectTimeoutSeconds, TimeUnit.SECONDS)
            .readTimeout(readTimeoutSeconds, TimeUnit.SECONDS)
            .addInterceptor(AuthInterceptor { token })

        if (includeLogging) {
            builder.addInterceptor(
                HttpLoggingInterceptor().apply {
                    level = HttpLoggingInterceptor.Level.BASIC
                },
            )
        }

        val certificateChainPem = settingsRepository
            ?.cloudflareDeviceCertificateChainPem
            ?.first()
            ?.trim()
            .orEmpty()
        if (certificateChainPem.isNotBlank()) {
            runCatching {
                val manager = deviceKeyManager ?: DeviceKeyManager()
                loadClientCertificate(
                    alias = manager.deviceKeyAlias(),
                    certificateChainPem = certificateChainPem,
                )
            }
                .onSuccess { sslConfig ->
                    sslConfig?.let { (sslSocketFactory, trustManager) ->
                        builder.sslSocketFactory(sslSocketFactory, trustManager)
                    }
                }
                .onFailure { error ->
                    Log.w(TAG, "Unable to load SM Cloudflare client certificate", error)
                }
        }

        return builder.build()
    }

    private fun loadClientCertificate(
        alias: String,
        certificateChainPem: String,
    ): Pair<SSLSocketFactory, X509TrustManager>? {
        val certificateChain = decodeCertificates(certificateChainPem)
        if (certificateChain.isEmpty()) {
            return null
        }
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        val privateKey = runCatching { keyStore.getKey(alias, null) as? PrivateKey }.getOrNull()
            ?: return null

        val keyManager = SingleAliasKeyManager(
            alias = alias,
            privateKey = privateKey,
            certificateChain = certificateChain.toTypedArray(),
        )

        val trustManagerFactory = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm()).apply {
            init(null as KeyStore?)
        }
        val trustManager = trustManagerFactory.trustManagers
            .filterIsInstance<X509TrustManager>()
            .singleOrNull()
            ?: return null

        val sslContext = SSLContext.getInstance("TLS").apply {
            init(
                arrayOf(keyManager),
                arrayOf(trustManager),
                SecureRandom(),
            )
        }

        return sslContext.socketFactory to trustManager
    }

    private fun decodeCertificates(certificateChainPem: String): List<X509Certificate> {
        val certificateFactory = CertificateFactory.getInstance("X.509")
        return certificateFactory.generateCertificates(ByteArrayInputStream(certificateChainPem.toByteArray()))
            .filterIsInstance<X509Certificate>()
    }

    private companion object {
        const val TAG = "HttpClientFactory"
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
    }

    private class SingleAliasKeyManager(
        private val alias: String,
        private val privateKey: PrivateKey,
        private val certificateChain: Array<X509Certificate>,
    ) : X509ExtendedKeyManager() {
        private val certificateKeyType = certificateChain
            .firstOrNull()
            ?.publicKey
            ?.algorithm
            ?.uppercase()
            .orEmpty()

        override fun getClientAliases(keyType: String?, issuers: Array<out Principal>?): Array<String> =
            if (supportsKeyType(keyType)) arrayOf(alias) else emptyArray()

        override fun chooseClientAlias(
            keyType: Array<out String>?,
            issuers: Array<out Principal>?,
            socket: Socket?,
        ): String? = alias.takeIf { keyType.orEmpty().any(::supportsKeyType) }

        override fun getServerAliases(keyType: String?, issuers: Array<out Principal>?): Array<String>? = null

        override fun chooseServerAlias(
            keyType: String?,
            issuers: Array<out Principal>?,
            socket: Socket?,
        ): String? = null

        override fun getCertificateChain(requestedAlias: String?): Array<X509Certificate>? =
            certificateChain.takeIf { requestedAlias == alias }

        override fun getPrivateKey(requestedAlias: String?): PrivateKey? =
            privateKey.takeIf { requestedAlias == alias }

        override fun chooseEngineClientAlias(
            keyType: Array<out String>?,
            issuers: Array<out Principal>?,
            engine: SSLEngine?,
        ): String? = alias.takeIf { keyType.orEmpty().any(::supportsKeyType) }

        override fun chooseEngineServerAlias(
            keyType: String?,
            issuers: Array<out Principal>?,
            engine: SSLEngine?,
        ): String? = null

        private fun supportsKeyType(keyType: String?): Boolean {
            val requested = keyType?.uppercase().orEmpty()
            return when (certificateKeyType) {
                "EC", "ECDSA" -> requested == "EC" || requested == "ECDSA" || requested.startsWith("EC_")
                "RSA" -> requested == "RSA" || requested == "RSASSA-PSS" || requested.startsWith("RSA_")
                else -> requested == certificateKeyType
            }
        }
    }
}
