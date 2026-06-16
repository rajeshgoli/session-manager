package li.rajeshgo.sm.data.security

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.MessageDigest
import java.security.PrivateKey
import java.security.Signature
import java.security.spec.ECGenParameterSpec

data class DeviceProof(
    val deviceKeyId: String,
    val timestamp: String,
    val nonce: String,
    val signature: String,
)

class DeviceKeyManager {
    private val keyStore: KeyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }

    fun deviceKeyId(): String {
        val publicKey = ensureKeyPair().certificate.publicKey.encoded
        val digest = MessageDigest.getInstance("SHA-256").digest(publicKey)
        return "android-${digest.take(8).joinToString("") { "%02x".format(it.toInt() and 0xff) }}"
    }

    fun publicKeyPem(): String {
        val publicKey = ensureKeyPair().certificate.publicKey.encoded
        val body = Base64.encodeToString(publicKey, Base64.NO_WRAP)
            .chunked(64)
            .joinToString("\n")
        return "-----BEGIN PUBLIC KEY-----\n$body\n-----END PUBLIC KEY-----"
    }

    fun signTicketRequest(
        method: String,
        path: String,
        sessionId: String,
        actorEmail: String,
    ): DeviceProof {
        val timestamp = (System.currentTimeMillis() / 1000L).toString()
        val nonce = randomNonce()
        val keyId = deviceKeyId()
        val message = listOf(
            "SM-MOBILE-TERMINAL-TICKET-V1",
            method.uppercase(),
            path,
            sessionId,
            actorEmail.lowercase(),
            keyId,
            timestamp,
            nonce,
        ).joinToString("\n")
        return DeviceProof(
            deviceKeyId = keyId,
            timestamp = timestamp,
            nonce = nonce,
            signature = sign(message),
        )
    }

    fun signWebSocketAuth(
        ticketId: String,
        sessionId: String,
        actorEmail: String,
        deviceKeyId: String,
        nonce: String,
    ): String {
        val message = listOf(
            "SM-MOBILE-TERMINAL-WS-V1",
            ticketId,
            sessionId,
            actorEmail.lowercase(),
            deviceKeyId,
            nonce,
        ).joinToString("\n")
        return sign(message)
    }

    private fun sign(message: String): String {
        val privateKey = ensurePrivateKey()
        val signer = Signature.getInstance("SHA256withECDSA")
        signer.initSign(privateKey)
        signer.update(message.toByteArray(Charsets.UTF_8))
        return Base64.encodeToString(signer.sign(), Base64.NO_WRAP)
    }

    private fun ensurePrivateKey(): PrivateKey {
        ensureKeyPair()
        return keyStore.getKey(KEY_ALIAS, null) as PrivateKey
    }

    private fun ensureKeyPair(): KeyStore.PrivateKeyEntry {
        keyStore.getEntry(KEY_ALIAS, null)?.let { entry ->
            if (entry is KeyStore.PrivateKeyEntry) {
                return entry
            }
        }
        val generator = KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, ANDROID_KEYSTORE)
        val spec = KeyGenParameterSpec.Builder(
            KEY_ALIAS,
            KeyProperties.PURPOSE_SIGN or KeyProperties.PURPOSE_VERIFY,
        )
            .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
            .setDigests(KeyProperties.DIGEST_SHA256)
            .setUserAuthenticationRequired(false)
            .build()
        generator.initialize(spec)
        generator.generateKeyPair()
        return keyStore.getEntry(KEY_ALIAS, null) as KeyStore.PrivateKeyEntry
    }

    private fun randomNonce(): String {
        val bytes = ByteArray(18)
        java.security.SecureRandom().nextBytes(bytes)
        return Base64.encodeToString(bytes, Base64.URL_SAFE or Base64.NO_WRAP or Base64.NO_PADDING)
    }

    private companion object {
        private const val ANDROID_KEYSTORE = "AndroidKeyStore"
        private const val KEY_ALIAS = "li.rajeshgo.sm.mobile_terminal_device_key.v1"
    }
}
