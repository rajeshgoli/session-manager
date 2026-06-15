package li.rajeshgo.sm.data.repository

import android.os.Build
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import li.rajeshgo.sm.data.model.DeviceEnrollmentRequest
import li.rajeshgo.sm.data.model.DeviceEnrollmentResponse
import li.rajeshgo.sm.data.remote.HttpClientFactory
import li.rajeshgo.sm.data.security.DeviceKeyManager
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import java.net.URI

data class DeviceEnrollmentResult(
    val deviceId: String,
    val deviceName: String?,
    val expiresAt: String?,
)

class DeviceEnrollmentRepository(
    private val settingsRepository: SettingsRepository,
    private val deviceKeyManager: DeviceKeyManager,
) {
    private val json = Json { ignoreUnknownKeys = true; coerceInputValues = true }
    private val httpClientFactory = HttpClientFactory(settingsRepository, deviceKeyManager)

    suspend fun enrollFromQr(qrContents: String): DeviceEnrollmentResult = withContext(Dispatchers.IO) {
        val enrollmentUrl = enrollmentUrlFromQrContents(qrContents)
        val requestBody = DeviceEnrollmentRequest(
            deviceId = deviceKeyManager.deviceKeyId(),
            deviceName = deviceName(),
            csrPem = deviceKeyManager.certificateSigningRequestPem(),
            publicKeyPem = deviceKeyManager.publicKeyPem(),
        )
        val request = Request.Builder()
            .url(enrollmentUrl)
            .post(
                json.encodeToString(DeviceEnrollmentRequest.serializer(), requestBody)
                    .toRequestBody("application/json".toMediaType()),
            )
            .build()
        val response = httpClientFactory.create(
            includeLogging = false,
            connectTimeoutSeconds = 10,
            readTimeoutSeconds = 30,
        )
            .newCall(request)
            .execute()
        response.use { result ->
            val body = result.body?.string().orEmpty()
            if (!result.isSuccessful) {
                throw IllegalStateException(enrollmentErrorMessage(body, result.code))
            }
            val payload = json.decodeFromString(DeviceEnrollmentResponse.serializer(), body)
            check(payload.deviceId.trim() == requestBody.deviceId) {
                "Enrollment response device id did not match this device"
            }
            check(deviceKeyManager.certificateChainMatchesDeviceKey(payload.certificateChainPem)) {
                "Enrollment certificate does not match this device key"
            }
            settingsRepository.saveCloudflareDeviceCertificateChainPem(payload.certificateChainPem)
            DeviceEnrollmentResult(
                deviceId = payload.deviceId,
                deviceName = payload.deviceName,
                expiresAt = payload.expiresAt,
            )
        }
    }

    private fun deviceName(): String {
        val manufacturer = Build.MANUFACTURER.trim()
        val model = Build.MODEL.trim()
        return listOf(manufacturer, model)
            .filter { it.isNotBlank() }
            .joinToString(" ")
            .ifBlank { "Android device" }
    }

    private fun enrollmentErrorMessage(body: String, statusCode: Int): String {
        val trimmed = body.trim()
        if (trimmed.isBlank()) {
            return "Enrollment failed (HTTP $statusCode)"
        }
        return runCatching {
            val obj = json.parseToJsonElement(trimmed).jsonObject
            listOf("detail", "message", "error")
                .firstNotNullOfOrNull { key ->
                    obj[key]?.jsonPrimitive?.content?.trim()?.takeIf { it.isNotBlank() }
                }
        }.getOrNull()
            ?: trimmed.takeUnless { it.startsWith("<!DOCTYPE") || it.startsWith("<html", ignoreCase = true) }
            ?: "Enrollment failed (HTTP $statusCode)"
    }

    companion object {
        fun enrollmentUrlFromQrContents(qrContents: String): String {
            val raw = qrContents.trim()
            require(raw.isNotBlank()) { "Enrollment QR code was empty" }
            val candidate = if (raw.startsWith("{")) {
                val payload = Json.parseToJsonElement(raw).jsonObject
                listOf("enrollment_url", "pairing_url", "url")
                    .firstNotNullOfOrNull { key ->
                        payload[key]?.jsonPrimitive?.content?.trim()?.takeIf { it.isNotBlank() }
                    }
                    ?: throw IllegalArgumentException("Enrollment QR code is missing enrollment_url")
            } else {
                raw
            }
            val uri = runCatching { URI(candidate) }.getOrNull()
                ?: throw IllegalArgumentException("Enrollment URL is not valid")
            val scheme = uri.scheme?.lowercase()
            require(scheme == "https" || scheme == "http") {
                "Enrollment URL must be http(s)"
            }
            require(!uri.host.isNullOrBlank()) {
                "Enrollment URL must include a host"
            }
            return candidate
        }
    }
}
