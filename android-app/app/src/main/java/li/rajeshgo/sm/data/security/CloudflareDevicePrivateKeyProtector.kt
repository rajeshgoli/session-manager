package li.rajeshgo.sm.data.security

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyStore
import java.util.Base64
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

class CloudflareDevicePrivateKeyProtector {
    fun protect(privateKeyPkcs8: String): String {
        val trimmed = privateKeyPkcs8.trim()
        if (trimmed.isBlank() || isProtectedPayload(trimmed)) {
            return trimmed
        }
        val cipher = Cipher.getInstance(TRANSFORMATION).apply {
            init(Cipher.ENCRYPT_MODE, getOrCreateSecretKey())
        }
        val ciphertext = cipher.doFinal(trimmed.toByteArray(Charsets.UTF_8))
        return "$PAYLOAD_PREFIX:${encode(cipher.iv)}:${encode(ciphertext)}"
    }

    fun unprotect(storedPrivateKey: String): String? {
        val trimmed = storedPrivateKey.trim()
        if (trimmed.isBlank()) {
            return ""
        }
        if (!isProtectedPayload(trimmed)) {
            return trimmed
        }
        return runCatching {
            val parts = trimmed.split(':', limit = 3)
            require(parts.size == 3 && parts[0] == PAYLOAD_PREFIX)
            val iv = decode(parts[1])
            val ciphertext = decode(parts[2])
            val cipher = Cipher.getInstance(TRANSFORMATION).apply {
                init(Cipher.DECRYPT_MODE, getOrCreateSecretKey(), GCMParameterSpec(GCM_TAG_BITS, iv))
            }
            String(cipher.doFinal(ciphertext), Charsets.UTF_8).trim()
        }.getOrNull()
    }

    fun isProtectedPayload(value: String): Boolean =
        value.trim().startsWith("$PAYLOAD_PREFIX:")

    private fun getOrCreateSecretKey(): SecretKey {
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        (keyStore.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }

        val keyGenerator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
        val spec = KeyGenParameterSpec.Builder(
            KEY_ALIAS,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .build()
        keyGenerator.init(spec)
        return keyGenerator.generateKey()
    }

    private fun encode(bytes: ByteArray): String =
        Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)

    private fun decode(value: String): ByteArray =
        Base64.getUrlDecoder().decode(value)

    private companion object {
        private const val ANDROID_KEYSTORE = "AndroidKeyStore"
        private const val KEY_ALIAS = "li.rajeshgo.sm.cloudflare_device_private_key.wrap"
        private const val PAYLOAD_PREFIX = "smcfpk1"
        private const val TRANSFORMATION = "AES/GCM/NoPadding"
        private const val GCM_TAG_BITS = 128
    }
}
