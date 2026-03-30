package li.rajeshgo.sm.data.repository

import android.content.Context
import android.content.Intent
import androidx.core.content.FileProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import retrofit2.HttpException
import retrofit2.Retrofit
import com.jakewharton.retrofit2.converter.kotlinx.serialization.asConverterFactory
import li.rajeshgo.sm.BuildConfig
import li.rajeshgo.sm.data.model.AppArtifactMetadata
import li.rajeshgo.sm.data.remote.ApiService
import java.io.File
import java.io.IOException
import java.security.MessageDigest
import java.util.concurrent.TimeUnit

data class AvailableAppUpdate(
    val artifactHash: String,
    val versionName: String,
    val uploadedAt: String?,
)

class AppUpdateRepository(
    private val context: Context,
    private val settingsRepository: SettingsRepository,
) {
    private val json = Json { ignoreUnknownKeys = true; coerceInputValues = true }

    @Volatile
    private var cachedCurrentArtifactHash: String? = null

    suspend fun getAvailableUpdate(): AvailableAppUpdate? {
        val serverUrl = settingsRepository.serverUrl.first().trimEnd('/')
        if (serverUrl.isBlank()) {
            return null
        }
        val metadata = fetchMetadata(serverUrl) ?: return null
        val serverArtifactHash = metadata.artifactHash?.trim()?.lowercase() ?: return null
        val currentArtifactHash = currentBuildArtifactHash()
        if (serverArtifactHash == currentArtifactHash) {
            return null
        }
        if (settingsRepository.dismissedUpdateArtifactHash.first() == serverArtifactHash) {
            return null
        }
        return AvailableAppUpdate(
            artifactHash = serverArtifactHash,
            versionName = metadata.versionName ?: serverArtifactHash,
            uploadedAt = metadata.uploadedAt,
        )
    }

    suspend fun dismissUpdate(artifactHash: String) {
        settingsRepository.saveDismissedUpdateArtifactHash(artifactHash)
    }

    suspend fun downloadUpdate(update: AvailableAppUpdate): File = withContext(Dispatchers.IO) {
        val serverUrl = settingsRepository.serverUrl.first().trimEnd('/')
        val request = Request.Builder()
            .url("$serverUrl/apps/session-manager-android/${update.artifactHash}.apk")
            .build()

        val updatesDir = File(context.cacheDir, "updates").apply { mkdirs() }
        val apkFile = File(updatesDir, "session-manager-${update.artifactHash}.apk")

        okHttpClient().newCall(request).execute().use { response ->
            if (!response.isSuccessful) {
                throw IOException("Update download failed: HTTP ${response.code}")
            }
            val responseBody = response.body ?: throw IOException("Update download returned no body")
            responseBody.byteStream().use { input ->
                apkFile.outputStream().use { output ->
                    input.copyTo(output)
                }
            }
        }

        apkFile
    }

    fun launchInstaller(apkFile: File) {
        val uri = FileProvider.getUriForFile(
            context,
            "${context.packageName}.fileprovider",
            apkFile,
        )
        val intent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, "application/vnd.android.package-archive")
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        context.startActivity(intent)
    }

    private suspend fun fetchMetadata(serverUrl: String): AppArtifactMetadata? = withContext(Dispatchers.IO) {
        try {
            apiService(serverUrl).getAppArtifactMetadata("session-manager-android")
        } catch (error: HttpException) {
            if (error.code() == 404) {
                null
            } else {
                throw error
            }
        }
    }

    private suspend fun currentBuildArtifactHash(): String {
        cachedCurrentArtifactHash?.let { return it }

        val configuredHash = BuildConfig.SM_APK_HASH.trim().lowercase()
        if (configuredHash.isNotEmpty()) {
            cachedCurrentArtifactHash = configuredHash
            return configuredHash
        }

        return withContext(Dispatchers.IO) {
            val computedHash = sha256Prefix(File(context.packageCodePath))
            cachedCurrentArtifactHash = computedHash
            computedHash
        }
    }

    private fun sha256Prefix(file: File): String {
        val digest = MessageDigest.getInstance("SHA-256")
        file.inputStream().use { input ->
            val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
            while (true) {
                val bytesRead = input.read(buffer)
                if (bytesRead <= 0) {
                    break
                }
                digest.update(buffer, 0, bytesRead)
            }
        }
        return digest.digest()
            .joinToString(separator = "") { byte -> "%02x".format(byte) }
            .take(8)
    }

    private fun apiService(serverUrl: String): ApiService {
        val retrofit = Retrofit.Builder()
            .baseUrl("$serverUrl/")
            .client(okHttpClient())
            .addConverterFactory(json.asConverterFactory("application/json".toMediaType()))
            .build()
        return retrofit.create(ApiService::class.java)
    }

    private fun okHttpClient(): OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(10, TimeUnit.SECONDS)
        .readTimeout(30, TimeUnit.SECONDS)
        .build()
}
