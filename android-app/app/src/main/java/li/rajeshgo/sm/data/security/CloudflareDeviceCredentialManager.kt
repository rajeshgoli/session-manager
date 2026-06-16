package li.rajeshgo.sm.data.security

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
import java.security.KeyFactory
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import java.security.interfaces.RSAPrivateCrtKey
import java.security.interfaces.RSAPublicKey
import java.security.spec.PKCS8EncodedKeySpec
import java.util.Base64
import java.util.UUID

data class CloudflareDeviceCredentialRequest(
    val csrPem: String,
    val privateKeyPkcs8: String,
)

class CloudflareDeviceCredentialManager {
    fun newCredentialAlias(): String = "$KEY_ALIAS_PREFIX${UUID.randomUUID()}"

    fun certificateSigningRequest(alias: String, commonName: String): CloudflareDeviceCredentialRequest {
        val keyPair = generateKeyPair(alias)
        val subject = X500Name("CN=${commonName.ifBlank { "Session Manager Android Device" }}")
        val builder = JcaPKCS10CertificationRequestBuilder(subject, keyPair.public)
        val signer = JcaContentSignerBuilder("SHA256withRSA").build(keyPair.private)
        val request: PKCS10CertificationRequest = builder.build(signer)
        val writer = StringWriter()
        JcaPEMWriter(writer).use { pemWriter ->
            pemWriter.writeObject(request)
        }
        return CloudflareDeviceCredentialRequest(
            csrPem = writer.toString(),
            privateKeyPkcs8 = Base64.getEncoder().encodeToString(keyPair.private.encoded),
        )
    }

    fun certificateChainMatchesPrivateKey(certificateChainPem: String, privateKeyPkcs8: String): Boolean {
        val leaf = decodeCertificates(certificateChainPem).firstOrNull() ?: return false
        val leafPublicKey = leaf.publicKey as? RSAPublicKey ?: return false
        val privateKey = decodePrivateKey(privateKeyPkcs8) as? RSAPrivateCrtKey ?: return false
        return leafPublicKey.modulus == privateKey.modulus &&
            leafPublicKey.publicExponent == privateKey.publicExponent
    }

    fun deleteCredential(alias: String) {
        if (alias.isBlank()) {
            return
        }
        runCatching {
            KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }.deleteEntry(alias)
        }
    }

    fun decodePrivateKey(privateKeyPkcs8: String) =
        runCatching {
            val keyBytes = Base64.getDecoder().decode(privateKeyPkcs8.trim())
            KeyFactory.getInstance("RSA").generatePrivate(PKCS8EncodedKeySpec(keyBytes))
        }.getOrNull()

    private fun generateKeyPair(alias: String): KeyPair {
        // Remove any stale AndroidKeyStore entry from earlier builds. The Cloudflare
        // mTLS key itself is software-backed because Conscrypt can fail client auth
        // signing with AndroidKeyStore RSA keys on affected devices.
        deleteCredential(alias)
        val keyPairGenerator = KeyPairGenerator.getInstance("RSA")
        keyPairGenerator.initialize(2048)
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
