package li.rajeshgo.sm.data.remote

import li.rajeshgo.sm.data.model.ActivityActionsResponse
import li.rajeshgo.sm.data.model.AppArtifactMetadata
import li.rajeshgo.sm.data.model.AuthSessionResponse
import li.rajeshgo.sm.data.model.ClientBootstrapResponse
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.ContextSnapshotResponse
import li.rajeshgo.sm.data.model.DeviceGoogleAuthRequest
import li.rajeshgo.sm.data.model.DeviceGoogleAuthResponse
import li.rajeshgo.sm.data.model.RetireSessionRequest
import li.rajeshgo.sm.data.model.RetireSessionResponse
import li.rajeshgo.sm.data.model.MobileAttachTicketRequest
import li.rajeshgo.sm.data.model.MobileAttachTicketResponse
import li.rajeshgo.sm.data.model.OutputResponse
import li.rajeshgo.sm.data.model.SessionListResponse
import li.rajeshgo.sm.data.model.StudioSshStatusResponse
import li.rajeshgo.sm.data.model.StudioSshToggleRequest
import li.rajeshgo.sm.data.model.ToolCallsResponse
import li.rajeshgo.sm.data.model.WhatRequestBody
import li.rajeshgo.sm.data.model.WhatRequestRecord
import retrofit2.http.Body
import retrofit2.http.DELETE
import retrofit2.http.GET
import retrofit2.http.HTTP
import retrofit2.http.Header
import retrofit2.http.POST
import retrofit2.http.PUT
import retrofit2.http.Path
import retrofit2.http.Query

interface ApiService {
    @GET("client/host-status")
    suspend fun getHostStatus(): li.rajeshgo.sm.data.model.HostStatus

    @GET("client/analytics/spend")
    suspend fun getAnalyticsSpend(@Query("provider") provider: String?, @Query("range") range: String): li.rajeshgo.sm.data.model.SpendReport

    @GET("client/analytics/time")
    suspend fun getAnalyticsTime(@Query("range") range: String): li.rajeshgo.sm.data.model.TimeReport

    @GET("client/queue")
    suspend fun getQueue(): li.rajeshgo.sm.data.model.QueueOverview

    @GET("client/queue/stats")
    suspend fun getQueueStats(@Query("hours") hours: Int): li.rajeshgo.sm.data.model.QueueStats

    @GET("client/utilization/series")
    suspend fun getUtilizationSeries(@Query("hours") hours: Int): li.rajeshgo.sm.data.model.UtilizationSeries

    @GET("client/queue/jobs/{job_id}/start-check")
    suspend fun getQueueStartCheck(@Path("job_id") jobId: String): li.rajeshgo.sm.data.model.QueueStartCheck

    @POST("client/queue/jobs/{job_id}/start")
    suspend fun forceStartQueueJob(@Path("job_id") jobId: String): li.rajeshgo.sm.data.model.SessionJob

    @POST("queue-jobs/{job_id}/cancel")
    suspend fun cancelQueueJob(
        @Path("job_id") jobId: String,
        @Body request: li.rajeshgo.sm.data.model.CancelQueueJobBody,
    ): li.rajeshgo.sm.data.model.SessionJob

    @GET("queue-jobs/{job_id}/log")
    suspend fun getQueueJobLog(
        @Path("job_id") jobId: String,
        @Query("lines") lines: Int,
    ): li.rajeshgo.sm.data.model.QueueJobLog

    @GET("client/session-models")
    suspend fun getSessionModels(@Query("provider") provider: String, @Query("working_dir") workingDir: String): li.rajeshgo.sm.data.model.SessionModelsResponse

    @POST("sessions")
    suspend fun createSession(@Body request: li.rajeshgo.sm.data.model.CreateSessionRequest): li.rajeshgo.sm.data.model.CreatedSession

    @GET("queue-jobs?include_terminal=true&terminal_limit_per_session=10&current_sessions_only=true")
    suspend fun getSessionJobs(): li.rajeshgo.sm.data.model.SessionJobsResponse

    @GET("session-obligations")
    suspend fun getSessionObligations(): li.rajeshgo.sm.data.model.SessionObligationsResponse

    @GET("client/bootstrap")
    suspend fun getBootstrap(): ClientBootstrapResponse

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

    @GET("sessions/{session_id}/handoff-policy")
    suspend fun getHandoffPolicy(@Path("session_id") sessionId: String): li.rajeshgo.sm.data.model.HandoffPolicy

    @retrofit2.http.PUT("sessions/{session_id}/handoff-policy")
    suspend fun setHandoffPolicy(
        @Path("session_id") sessionId: String,
        @Body patch: kotlinx.serialization.json.JsonObject,
    ): li.rajeshgo.sm.data.model.HandoffPolicy

    @GET("handoff-defaults")
    suspend fun getHandoffDefaults(): li.rajeshgo.sm.data.model.HandoffDefaults

    @retrofit2.http.PUT("handoff-defaults")
    suspend fun setHandoffDefaults(@Body patch: kotlinx.serialization.json.JsonObject): li.rajeshgo.sm.data.model.HandoffDefaults

    @POST("client/sessions/{session_id}/attach-ticket")
    suspend fun createMobileAttachTicket(
        @Path("session_id") sessionId: String,
        @Header("X-SM-Device-Key-Id") deviceKeyId: String,
        @Header("X-SM-Device-Timestamp") timestamp: String,
        @Header("X-SM-Device-Nonce") nonce: String,
        @Header("X-SM-Device-Signature") signature: String,
        @Body request: MobileAttachTicketRequest = MobileAttachTicketRequest(),
    ): MobileAttachTicketResponse

    @GET("admin/studio-ssh")
    suspend fun getStudioSshStatus(): StudioSshStatusResponse

    @POST("admin/studio-ssh")
    suspend fun setStudioSsh(@Body request: StudioSshToggleRequest): StudioSshStatusResponse

    @GET("sessions/{session_id}/output")
    suspend fun getSessionOutput(
        @Path("session_id") sessionId: String,
        @Query("lines") lines: Int = 10,
        @Query("rendered") rendered: Boolean = true,
    ): OutputResponse

    @GET("sessions/{session_id}/context")
    suspend fun getSessionContext(
        @Path("session_id") sessionId: String,
    ): ContextSnapshotResponse

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

    @POST("sessions/{session_id}/retire")
    suspend fun retireSession(
        @Path("session_id") sessionId: String,
        @Body request: RetireSessionRequest = RetireSessionRequest(),
    ): RetireSessionResponse

    @POST("sessions/{session_id}/what")
    suspend fun createWhatRequest(
        @Path("session_id") sessionId: String,
        @Body request: WhatRequestBody,
    ): WhatRequestRecord

    @GET("btw-requests/{request_id}")
    suspend fun getWhatRequest(
        @Path("request_id") requestId: String,
    ): WhatRequestRecord

    @GET("client/follows")
    suspend fun getFollows(): li.rajeshgo.sm.data.model.FollowsResponse

    @POST("sessions/{session_id}/follow")
    suspend fun followSession(
        @Path("session_id") sessionId: String,
        @Body request: li.rajeshgo.sm.data.model.FollowSessionRequest,
    ): li.rajeshgo.sm.data.model.OwnerFollow

    @DELETE("sessions/{session_id}/follow")
    suspend fun unfollowSession(@Path("session_id") sessionId: String)

    @POST("queue-jobs/{job_id}/follow")
    suspend fun followJob(@Path("job_id") jobId: String): li.rajeshgo.sm.data.model.OwnerFollow

    @DELETE("queue-jobs/{job_id}/follow")
    suspend fun unfollowJob(@Path("job_id") jobId: String)

    @POST("client/follows/{follow_id}/ack")
    suspend fun ackFollow(@Path("follow_id") followId: String)

    /** The phone showed a message or review notification (sm#1580). */
    @POST("client/notices/{notice_id}/ack")
    suspend fun ackNotice(@Path("notice_id") noticeId: String)

    @PUT("client/push-token")
    suspend fun registerPushToken(@Body request: li.rajeshgo.sm.data.model.PushTokenRequest)

    @HTTP(method = "DELETE", path = "client/push-token", hasBody = true)
    suspend fun deletePushToken(@Body request: li.rajeshgo.sm.data.model.DeletePushTokenRequest)

    @POST("client/push/test")
    suspend fun sendTestPush(): li.rajeshgo.sm.data.model.TestPushResponse

    /** The owner's Inbox (sm#1647); `filter` is `open`, `docs` or `done`. */
    @GET("inbox?format=json")
    suspend fun getInbox(@Query("filter") filter: String): li.rajeshgo.sm.data.model.InboxResponse

    /** Done on a thread: clears its open ask without messaging the agent. */
    @POST("inbox/done")
    suspend fun markInboxDone(@Body request: li.rajeshgo.sm.data.model.InboxDoneRequest)

    /** Guestbook entries (sm#1660), newest first; `before` is the previous page's `next_before`. */
    @GET("guestbook?format=json")
    suspend fun getGuestbook(
        @Query("repo") repo: String?,
        @Query("before") before: Long?,
    ): li.rajeshgo.sm.data.model.GuestbookResponse

    /** Agents no longer live (sm#1661), newest first; `before` is the previous page's `next_before`. */
    @GET("history/agents")
    suspend fun getAgentHistory(
        @Query("q") query: String?,
        @Query("before") before: String?,
    ): li.rajeshgo.sm.data.model.AgentHistoryResponse

    /** The board (sm#1665): lanes, their tickets and the Board count. */
    @GET("client/board")
    suspend fun getBoard(): li.rajeshgo.sm.data.model.BoardResponse

    @GET("client/board/badge")
    suspend fun getBoardBadge(): li.rajeshgo.sm.data.model.BoardBadge

    /** The owner saw the board: clears the Board count on web and phone. */
    @POST("client/board/seen")
    suspend fun markBoardSeen()

    /** Reads GitHub now; the pass runs in the background. */
    @POST("client/board/refresh")
    suspend fun refreshBoard()

    /** The full new order of active lanes, rank 1 first. */
    @PUT("client/board/order")
    suspend fun putBoardOrder(@Body request: li.rajeshgo.sm.data.model.BoardOrderRequest): li.rajeshgo.sm.data.model.BoardResponse

    @POST("client/board/lanes")
    suspend fun addBoardLane(@Body request: li.rajeshgo.sm.data.model.BoardLaneRequest): li.rajeshgo.sm.data.model.BoardLaneAdded

    @DELETE("client/board/lanes/{lane_id}")
    suspend fun endBoardLane(@Path("lane_id") laneId: Long): li.rajeshgo.sm.data.model.BoardResponse

    @GET("client/board/start-options")
    suspend fun getBoardStartOptions(
        @Query("repo") repo: String,
        @Query("number") number: Long,
    ): li.rajeshgo.sm.data.model.BoardStartOptions

    @POST("client/board/start")
    suspend fun startBoardTicket(@Body request: li.rajeshgo.sm.data.model.BoardStartRequest): li.rajeshgo.sm.data.model.BoardStarted

    /** Brings a stopped or retired agent back, as `sm restore` does. */
    @POST("sessions/{session_id}/restore")
    suspend fun restoreSession(@Path("session_id") sessionId: String): kotlinx.serialization.json.JsonObject
}
