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
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.net.InetSocketAddress
import java.net.URI
import java.net.Socket

data class DeviceEnrollmentResult(
    val deviceId: String,
    val deviceName: String?,
    val expiresAt: String?,
)

private data class EnrollmentHttpResponse(
    val statusCode: Int,
    val body: String,
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
        val requestJson = json.encodeToString(DeviceEnrollmentRequest.serializer(), requestBody)
        val response = postEnrollmentRequest(enrollmentUrl, requestJson)
        val body = response.body
        if (response.statusCode !in 200..299) {
            throw IllegalStateException(enrollmentErrorMessage(body, response.statusCode))
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

    private suspend fun postEnrollmentRequest(enrollmentUrl: String, requestJson: String): EnrollmentHttpResponse {
        val uri = URI(enrollmentUrl)
        return if (uri.scheme.equals("http", ignoreCase = true)) {
            postLocalHttpEnrollmentRequest(uri, requestJson)
        } else {
            postHttpsEnrollmentRequest(enrollmentUrl, requestJson)
        }
    }

    private suspend fun postHttpsEnrollmentRequest(enrollmentUrl: String, requestJson: String): EnrollmentHttpResponse {
        val request = Request.Builder()
            .url(enrollmentUrl)
            .post(
                requestJson.toRequestBody("application/json".toMediaType()),
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
            return EnrollmentHttpResponse(result.code, result.body?.string().orEmpty())
        }
    }

    private fun postLocalHttpEnrollmentRequest(uri: URI, requestJson: String): EnrollmentHttpResponse {
        val host = uri.host?.trim().orEmpty()
        require(isLocalPairingHttpHost(host)) {
            "HTTP enrollment URLs must use a local or private host"
        }
        val port = if (uri.port == -1) 80 else uri.port
        require(port in 1..65535) { "Enrollment URL port is invalid" }
        val target = buildString {
            append(uri.rawPath?.takeIf { it.isNotBlank() } ?: "/")
            uri.rawQuery?.takeIf { it.isNotBlank() }?.let { query ->
                append('?')
                append(query)
            }
        }
        val bodyBytes = requestJson.toByteArray(Charsets.UTF_8)
        Socket().use { socket ->
            socket.connect(InetSocketAddress(host, port), 10_000)
            socket.soTimeout = 30_000
            val requestHeaders = buildString {
                append("POST ")
                append(target)
                append(" HTTP/1.1\r\n")
                append("Host: ")
                append(host)
                if (uri.port != -1) {
                    append(':')
                    append(port)
                }
                append("\r\n")
                append("Accept: application/json\r\n")
                append("Content-Type: application/json\r\n")
                append("Content-Length: ")
                append(bodyBytes.size)
                append("\r\n")
                append("Connection: close\r\n")
                append("\r\n")
            }.toByteArray(Charsets.US_ASCII)
            val output = socket.getOutputStream()
            output.write(requestHeaders)
            output.write(bodyBytes)
            output.flush()
            socket.shutdownOutput()
            return readEnrollmentHttpResponse(socket.getInputStream())
        }
    }

    private fun readEnrollmentHttpResponse(input: InputStream): EnrollmentHttpResponse {
        val headerBytes = readHeaderBytes(input)
        val headerText = headerBytes.toString(Charsets.ISO_8859_1)
        val lines = headerText.split("\r\n")
        val statusCode = lines.firstOrNull()
            ?.split(' ', limit = 3)
            ?.getOrNull(1)
            ?.toIntOrNull()
            ?: throw IllegalStateException("Enrollment response did not include an HTTP status")
        val headers = lines.drop(1)
            .mapNotNull { line ->
                val separator = line.indexOf(':')
                if (separator <= 0) {
                    null
                } else {
                    line.substring(0, separator).trim().lowercase() to line.substring(separator + 1).trim()
                }
            }
            .toMap()
        val contentLength = headers["content-length"]?.toIntOrNull()
        val bodyBytes = if (contentLength != null && contentLength >= 0) {
            readExactly(input, contentLength)
        } else {
            input.readBytes()
        }
        return EnrollmentHttpResponse(statusCode, bodyBytes.toString(Charsets.UTF_8))
    }

    private fun readHeaderBytes(input: InputStream): ByteArray {
        val out = ByteArrayOutputStream()
        var matched = 0
        val delimiter = byteArrayOf('\r'.code.toByte(), '\n'.code.toByte(), '\r'.code.toByte(), '\n'.code.toByte())
        while (true) {
            val next = input.read()
            if (next == -1) {
                throw IllegalStateException("Enrollment response ended before HTTP headers")
            }
            out.write(next)
            matched = if (next.toByte() == delimiter[matched]) matched + 1 else if (next == '\r'.code) 1 else 0
            if (matched == delimiter.size) {
                val bytes = out.toByteArray()
                return bytes.copyOf(bytes.size - delimiter.size)
            }
            check(out.size() <= 64 * 1024) { "Enrollment response headers were too large" }
        }
    }

    private fun readExactly(input: InputStream, length: Int): ByteArray {
        val result = ByteArray(length)
        var offset = 0
        while (offset < length) {
            val read = input.read(result, offset, length - offset)
            if (read == -1) {
                throw IllegalStateException("Enrollment response body ended early")
            }
            offset += read
        }
        return result
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
            if (scheme == "http") {
                require(isLocalPairingHttpHost(uri.host)) {
                    "HTTP enrollment URLs must use a local or private host"
                }
            }
            return candidate
        }

        fun isLocalPairingHttpHost(host: String?): Boolean {
            val normalized = host
                ?.trim()
                ?.removePrefix("[")
                ?.removeSuffix("]")
                ?.lowercase()
                .orEmpty()
            if (normalized.isBlank()) {
                return false
            }
            if (normalized == "localhost" || normalized.endsWith(".local")) {
                return true
            }
            if (normalized == "::1" || normalized.startsWith("fe80:")) {
                return true
            }
            if (normalized.length >= 2 && normalized[0] == 'f' && normalized[1] in setOf('c', 'd')) {
                return true
            }
            val octets = normalized.split('.')
            if (octets.size != 4) {
                return false
            }
            val values = octets.map { part ->
                if (part.length > 1 && part.startsWith('0')) {
                    return false
                }
                part.toIntOrNull()?.takeIf { it in 0..255 } ?: return false
            }
            return values[0] == 10 ||
                values[0] == 127 ||
                (values[0] == 169 && values[1] == 254) ||
                (values[0] == 172 && values[1] in 16..31) ||
                (values[0] == 192 && values[1] == 168)
        }
    }
}
