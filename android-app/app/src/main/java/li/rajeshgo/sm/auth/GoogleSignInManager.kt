package li.rajeshgo.sm.auth

import android.content.Context
import androidx.credentials.ClearCredentialStateRequest
import androidx.credentials.CredentialManager
import androidx.credentials.CustomCredential
import androidx.credentials.GetCredentialRequest
import androidx.credentials.GetCredentialResponse
import com.google.android.libraries.identity.googleid.GetGoogleIdOption
import com.google.android.libraries.identity.googleid.GoogleIdTokenCredential
import com.google.android.libraries.identity.googleid.GoogleIdTokenParsingException

class GoogleSignInManager(private val context: Context) {
    private val credentialManager = CredentialManager.create(context)

    suspend fun getIdToken(serverClientId: String): Result<GoogleIdTokenCredential> {
        if (serverClientId.isBlank()) {
            return Result.failure(IllegalStateException("Google server client ID is not configured"))
        }
        return runCatching {
            val request = GetCredentialRequest.Builder()
                .addCredentialOption(
                    GetGoogleIdOption.Builder()
                        .setServerClientId(serverClientId)
                        .setFilterByAuthorizedAccounts(false)
                        .setAutoSelectEnabled(true)
                        .build()
                )
                .build()
            val result = credentialManager.getCredential(context, request)
            parseGoogleCredential(result)
        }
    }

    suspend fun clearCredentialState() {
        runCatching {
            credentialManager.clearCredentialState(ClearCredentialStateRequest())
        }
    }

    private fun parseGoogleCredential(result: GetCredentialResponse): GoogleIdTokenCredential {
        val credential = result.credential
        if (credential !is CustomCredential || credential.type != GoogleIdTokenCredential.TYPE_GOOGLE_ID_TOKEN_CREDENTIAL) {
            throw IllegalStateException("Google ID token credential not available")
        }
        return try {
            GoogleIdTokenCredential.createFrom(credential.data)
        } catch (error: GoogleIdTokenParsingException) {
            throw IllegalStateException("Failed to parse Google credential", error)
        }
    }
}
