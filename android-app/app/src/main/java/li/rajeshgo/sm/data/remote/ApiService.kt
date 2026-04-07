package li.rajeshgo.sm.data.remote

import li.rajeshgo.sm.data.model.ActivityActionsResponse
import li.rajeshgo.sm.data.model.AnalyticsSummary
import li.rajeshgo.sm.data.model.AppArtifactMetadata
import li.rajeshgo.sm.data.model.AuthSessionResponse
import li.rajeshgo.sm.data.model.ClientBootstrapResponse
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.DeviceGoogleAuthRequest
import li.rajeshgo.sm.data.model.DeviceGoogleAuthResponse
import li.rajeshgo.sm.data.model.KillSessionRequest
import li.rajeshgo.sm.data.model.KillSessionResponse
import li.rajeshgo.sm.data.model.OutputResponse
import li.rajeshgo.sm.data.model.RequestStatusResponse
import li.rajeshgo.sm.data.model.SessionListResponse
import li.rajeshgo.sm.data.model.ToolCallsResponse
import retrofit2.http.Body
import retrofit2.http.GET
import retrofit2.http.POST
import retrofit2.http.Path
import retrofit2.http.Query

interface ApiService {
    @GET("client/bootstrap")
    suspend fun getBootstrap(): ClientBootstrapResponse

    @GET("client/analytics/summary")
    suspend fun getAnalyticsSummary(): AnalyticsSummary

    @GET("apps/{app}/meta.json")
    suspend fun getAppArtifactMetadata(@Path("app") app: String): AppArtifactMetadata

    @GET("auth/session")
    suspend fun getAuthSession(): AuthSessionResponse

    @POST("auth/device/google")
    suspend fun exchangeGoogleToken(@Body request: DeviceGoogleAuthRequest): DeviceGoogleAuthResponse

    @GET("client/sessions")
    suspend fun getClientSessions(): SessionListResponse

    @GET("client/sessions/{session_id}")
    suspend fun getClientSession(@Path("session_id") sessionId: String): ClientSession

    @POST("client/request-status")
    suspend fun requestStatus(): RequestStatusResponse

    @GET("sessions/{session_id}/output")
    suspend fun getSessionOutput(
        @Path("session_id") sessionId: String,
        @Query("lines") lines: Int = 10,
    ): OutputResponse

    @GET("sessions/{session_id}/tool-calls")
    suspend fun getToolCalls(
        @Path("session_id") sessionId: String,
        @Query("limit") limit: Int = 10,
    ): ToolCallsResponse

    @GET("sessions/{session_id}/activity-actions")
    suspend fun getActivityActions(
        @Path("session_id") sessionId: String,
        @Query("limit") limit: Int = 10,
    ): ActivityActionsResponse

    @POST("sessions/{session_id}/kill")
    suspend fun killSession(
        @Path("session_id") sessionId: String,
        @Body request: KillSessionRequest = KillSessionRequest(),
    ): KillSessionResponse
}
