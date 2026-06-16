package li.rajeshgo.sm.data.security

import org.bouncycastle.asn1.x500.X500Name
import org.bouncycastle.cert.jcajce.JcaX509CertificateConverter
import org.bouncycastle.cert.jcajce.JcaX509v3CertificateBuilder
import org.bouncycastle.openssl.PEMParser
import org.bouncycastle.openssl.jcajce.JcaPEMKeyConverter
import org.bouncycastle.openssl.jcajce.JcaPEMWriter
import org.bouncycastle.operator.jcajce.JcaContentSignerBuilder
import org.bouncycastle.pkcs.PKCS10CertificationRequest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.StringReader
import java.io.StringWriter
import java.math.BigInteger
import java.security.KeyFactory
import java.security.PrivateKey
import java.security.interfaces.RSAPrivateCrtKey
import java.security.interfaces.RSAPublicKey
import java.security.spec.PKCS8EncodedKeySpec
import java.time.Instant
import java.util.Base64
import java.util.Date

class CloudflareDeviceCredentialManagerTest {
    private val manager = CloudflareDeviceCredentialManager()

    @Test
    fun credentialRequestPersistsSoftwareRsaKeyMatchingCsr() {
        val credential = manager.certificateSigningRequest("test-alias", "fixture-device")
        val csrPublicKey = csrPublicKey(credential.csrPem)
        val privateKey = privateKey(credential.privateKeyPkcs8)

        assertEquals("RSA", csrPublicKey.algorithm)
        assertEquals((csrPublicKey as RSAPublicKey).modulus, privateKey.modulus)
        assertEquals(csrPublicKey.publicExponent, privateKey.publicExponent)
    }

    @Test
    fun certificateChainMatchUsesSavedPrivateKeyMaterial() {
        val credential = manager.certificateSigningRequest("test-alias", "fixture-device")
        val certificatePem = selfSignedCertificatePem(credential)
        val otherCredential = manager.certificateSigningRequest("other-alias", "other-device")

        assertTrue(manager.certificateChainMatchesPrivateKey(certificatePem, credential.privateKeyPkcs8))
        assertFalse(manager.certificateChainMatchesPrivateKey(certificatePem, otherCredential.privateKeyPkcs8))
    }

    private fun csrPublicKey(csrPem: String) =
        PEMParser(StringReader(csrPem)).use { parser ->
            val csr = parser.readObject() as PKCS10CertificationRequest
            JcaPEMKeyConverter().getPublicKey(csr.subjectPublicKeyInfo)
        }

    private fun privateKey(privateKeyPkcs8: String): RSAPrivateCrtKey {
        val keyBytes = Base64.getDecoder().decode(privateKeyPkcs8)
        return KeyFactory.getInstance("RSA")
            .generatePrivate(PKCS8EncodedKeySpec(keyBytes)) as RSAPrivateCrtKey
    }

    private fun selfSignedCertificatePem(credential: CloudflareDeviceCredentialRequest): String {
        val publicKey = csrPublicKey(credential.csrPem)
        val privateKey: PrivateKey = privateKey(credential.privateKeyPkcs8)
        val now = Instant.parse("2026-06-16T00:00:00Z")
        val subject = X500Name("CN=fixture-device")
        val certificateHolder = JcaX509v3CertificateBuilder(
            subject,
            BigInteger.ONE,
            Date.from(now),
            Date.from(now.plusSeconds(3600)),
            subject,
            publicKey,
        ).build(JcaContentSignerBuilder("SHA256withRSA").build(privateKey))
        val certificate = JcaX509CertificateConverter().getCertificate(certificateHolder)
        val writer = StringWriter()
        JcaPEMWriter(writer).use { pemWriter ->
            pemWriter.writeObject(certificate)
        }
        return writer.toString()
    }
}
