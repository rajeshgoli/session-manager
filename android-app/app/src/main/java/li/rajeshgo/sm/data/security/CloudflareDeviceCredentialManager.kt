package li.rajeshgo.sm.data.security

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import org.bouncycastle.asn1.x500.X500Name
import org.bouncycastle.openssl.jcajce.JcaPEMWriter
import org.bouncycastle.operator.jcajce.JcaContentSignerBuilder
import org.bouncycastle.pkcs.PKCS10CertificationRequest
import org.bouncycastle.pkcs.jcajce.JcaPKCS10CertificationRequestBuilder
import java.io.ByteArrayInputStream
import java.io.StringWriter
import java.security.KeyPair
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import java.util.UUID

class CloudflareDeviceCredentialManager {
    fun newCredentialAlias(): String = "$KEY_ALIAS_PREFIX${UUID.randomUUID()}"

    fun certificateSigningRequestPem(alias: String, commonName: String): String {
        val keyPair = generateKeyPair(alias)
        val subject = X500Name("CN=${commonName.ifBlank { "Session Manager Android Device" }}")
        val builder = JcaPKCS10CertificationRequestBuilder(subject, keyPair.public)
        val signer = JcaContentSignerBuilder("SHA256withRSA").build(keyPair.private)
        val request: PKCS10CertificationRequest = builder.build(signer)
        val writer = StringWriter()
        JcaPEMWriter(writer).use { pemWriter ->
            pemWriter.writeObject(request)
        }
        return writer.toString()
    }

    fun certificateChainMatchesAlias(certificateChainPem: String, alias: String): Boolean {
        val leaf = decodeCertificates(certificateChainPem).firstOrNull() ?: return false
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        val storedPublicKey = keyStore.getCertificate(alias)?.publicKey ?: return false
        return leaf.publicKey.encoded.contentEquals(storedPublicKey.encoded)
    }

    fun deleteCredential(alias: String) {
        if (alias.isBlank()) {
            return
        }
        runCatching {
            KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }.deleteEntry(alias)
        }
    }

    private fun generateKeyPair(alias: String): KeyPair {
        deleteCredential(alias)
        val keyPairGenerator = KeyPairGenerator.getInstance(
            KeyProperties.KEY_ALGORITHM_RSA,
            ANDROID_KEYSTORE,
        )
        keyPairGenerator.initialize(
            KeyGenParameterSpec.Builder(
                alias,
                KeyProperties.PURPOSE_SIGN or KeyProperties.PURPOSE_VERIFY,
            )
                .setKeySize(2048)
                .setDigests(
                    KeyProperties.DIGEST_SHA256,
                    KeyProperties.DIGEST_SHA384,
                    KeyProperties.DIGEST_SHA512,
                )
                .setSignaturePaddings(
                    KeyProperties.SIGNATURE_PADDING_RSA_PKCS1,
                    KeyProperties.SIGNATURE_PADDING_RSA_PSS,
                )
                .setUserAuthenticationRequired(false)
                .build(),
        )
        return keyPairGenerator.generateKeyPair()
    }

    private fun decodeCertificates(certificateChainPem: String): List<X509Certificate> {
        if (certificateChainPem.isBlank()) {
            return emptyList()
        }
        val certificateFactory = CertificateFactory.getInstance("X.509")
        return certificateFactory.generateCertificates(ByteArrayInputStream(certificateChainPem.toByteArray()))
            .filterIsInstance<X509Certificate>()
    }

    private companion object {
        private const val ANDROID_KEYSTORE = "AndroidKeyStore"
        private const val KEY_ALIAS_PREFIX = "li.rajeshgo.sm.cloudflare_device."
    }
}
