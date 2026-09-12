package li.rajeshgo.sm.data.repository

import li.rajeshgo.sm.data.model.MobileAttachTicketResponse
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import okhttp3.ResponseBody.Companion.toResponseBody
import okhttp3.MediaType.Companion.toMediaType
import com.jakewharton.retrofit2.converter.kotlinx.serialization.asConverterFactory

class SessionManagerRepositoryTest {
    @Test
    fun jobRequestIncludesBoundedHistoryAndDecodesFinishedWork() = kotlinx.coroutines.runBlocking {
        val client = okhttp3.OkHttpClient.Builder().addInterceptor { chain ->
            val request = chain.request()
            assertEquals("true", request.url.queryParameter("include_terminal"))
            assertEquals("10", request.url.queryParameter("terminal_limit_per_session"))
            okhttp3.Response.Builder().request(request).protocol(okhttp3.Protocol.HTTP_1_1)
                .code(200).message("OK").body(
                    """{"jobs":[{"id":"done","state":"failed","notify_session_id":"agent","exit_code":1}]}""".toResponseBody("application/json".toMediaType()),
                ).build()
        }.build()
        val service = retrofit2.Retrofit.Builder().baseUrl("https://example.com/").client(client)
            .addConverterFactory(kotlinx.serialization.json.Json.asConverterFactory("application/json".toMediaType()))
            .build().create(li.rajeshgo.sm.data.remote.ApiService::class.java)
        val jobs = service.getSessionJobs().jobs
        assertEquals("failed", jobs.single().state)
        assertTrue(jobs.single().isAwaitedBy("agent"))
        assertEquals(1, jobs.single().exitCode)
    }

    @Test
    fun attachTicketLimitIsRetryableButAuthenticationIsNot() {
        val repository = SessionManagerRepository()
        fun failure(code: Int): Throwable = repository.classifyWriteFailure(
            retrofit2.HttpException(retrofit2.Response.error<Any>(code, "{}".toResponseBody())),
        )
        assertTrue(failure(429) is SessionManagerTransientException)
        assertTrue(failure(401) is SessionManagerAuthException)
        assertFalse(failure(403) is SessionManagerTransientException)
        assertFalse(failure(404) is SessionManagerTransientException)
    }

    @Test
    fun queueWorkBelongsToRecipientWhenRequesterDelegatesNotification() {
        val job = li.rajeshgo.sm.data.model.SessionJob("job", requesterSessionId = "sender", notifySessionId = "recipient")
        assertTrue(job.isAwaitedBy("recipient"))
        assertFalse(job.isAwaitedBy("sender"))
        assertTrue(job.copy(notifySessionId = null).isAwaitedBy("sender"))
        assertTrue(job.copy(notifySessionId = "").isAwaitedBy("sender"))
    }

    @Test
    fun forbiddenAccessDoesNotDiscardAnAuthenticatedDeviceLogin() {
        val failure: Throwable = forbiddenRequestFailure(IllegalStateException("gateway refused"))
        assertFalse(failure is SessionManagerAuthException)
        assertTrue(failure.message!!.contains("sign-in is saved"))
    }

    @Test
    fun retireFallbackErrorCopyUsesRetireLanguage() {
        assertEquals("Retire request failed", RETIRE_REQUEST_FAILED_MESSAGE)
        assertFalse(RETIRE_REQUEST_FAILED_MESSAGE.contains("kill", ignoreCase = true))
    }

    @Test
    fun activeWhatRequestIdExtractsConflictRequest() {
        assertEquals(
            "btw-cbc3c6fcccfeef9adc1023f795c3e971",
            activeWhatRequestId(
                "Target already has active sm what request btw-cbc3c6fcccfeef9adc1023f795c3e971"
            ),
        )
    }

    @Test
    fun activeWhatRequestIdRejectsUncorrelatedConflict() {
        assertNull(activeWhatRequestId("Another request is already active"))
    }

    @Test
    fun terminalCleanupRemovesAnsiAndControlBytes() {
        val raw = "\u001B[31mred\u001B[0m\n\u001B]0;title\u0007plain\u0000\ttext\u009B32mgreen"

        assertEquals("red\nplain\ttextgreen", stripTerminalControls(raw))
    }

    @Test
    fun mobileAttachTicketPathIncludesBaseUrlPathPrefix() {
        val repository = SessionManagerRepository()

        assertEquals(
            "/sm/client/sessions/abc123/attach-ticket",
            repository.mobileAttachTicketPath("https://example.com/sm/", "abc123"),
        )
    }

    @Test
    fun mobileAttachTicketPathUsesRootWhenBaseUrlHasNoPrefix() {
        val repository = SessionManagerRepository()

        assertEquals(
            "/client/sessions/abc123/attach-ticket",
            repository.mobileAttachTicketPath("https://example.com", "abc123"),
        )
    }

    @Test
    fun mobileAttachTicketPathPrefersAdvertisedTicketEndpoint() {
        val repository = SessionManagerRepository()

        assertEquals(
            "/proxy/client/sessions/abc123/attach-ticket",
            repository.mobileAttachTicketPath(
                "https://example.com/sm/",
                "abc123",
                "/proxy/client/sessions/abc123/attach-ticket",
            ),
        )
    }

    @Test
    fun mobileAttachTicketPathExtractsPathFromAbsoluteAdvertisedTicketEndpoint() {
        val repository = SessionManagerRepository()

        assertEquals(
            "/proxy/client/sessions/abc123/attach-ticket",
            repository.mobileAttachTicketPath(
                "https://example.com/sm/",
                "abc123",
                "https://api.example.com/proxy/client/sessions/abc123/attach-ticket",
            ),
        )
    }

    @Test
    fun mobileTerminalSocketRequestIncludesBearerToken() {
        val repository = SessionManagerRepository()
        val request = repository.mobileTerminalSocketRequest(ticket(), " smat_token ")

        assertEquals("Bearer smat_token", request.header("Authorization"))
        assertEquals("https://example.com/client/terminal", request.url.toString())
    }

    @Test
    fun mobileTerminalSocketRequestOmitsBlankBearerToken() {
        val repository = SessionManagerRepository()
        val request = repository.mobileTerminalSocketRequest(ticket(), " ")

        assertNull(request.header("Authorization"))
    }

    @Test
    fun mobileTerminalSocketRetryableFailureRecognizesUpgradeMisdirection() {
        val repository = SessionManagerRepository()

        assertTrue(repository.isRetryableMobileTerminalSocketFailure(404, "Expected HTTP 101 response"))
        assertTrue(repository.isRetryableMobileTerminalSocketFailure(426, null))
        assertTrue(repository.isRetryableMobileTerminalSocketFailure(503, null))
        assertTrue(repository.isRetryableMobileTerminalSocketFailure(null, "Expected HTTP 101 response but was 404"))
    }

    @Test
    fun mobileTerminalSocketRetryableFailureRejectsAuthAndGenericErrors() {
        val repository = SessionManagerRepository()

        assertFalse(repository.isRetryableMobileTerminalSocketFailure(401, "Unauthorized"))
        assertFalse(repository.isRetryableMobileTerminalSocketFailure(401, "Expected HTTP 101 response but was 401"))
        assertFalse(repository.isRetryableMobileTerminalSocketFailure(403, "Expected HTTP 101 response but was 403"))
        assertFalse(repository.isRetryableMobileTerminalSocketFailure(null, "timeout"))
    }

    @Test
    fun deviceEnrollmentQrAcceptsPlainPairingUrl() {
        assertEquals(
            "http://studio.local:8420/client/mobile-terminal/enroll/abc",
            DeviceEnrollmentRepository.enrollmentUrlFromQrContents(
                " http://studio.local:8420/client/mobile-terminal/enroll/abc ",
            ),
        )
    }

    @Test
    fun deviceEnrollmentQrAcceptsPrivateHttpPairingUrl() {
        assertEquals(
            "http://192.168.4.31:19192/client/mobile-terminal/enroll/abc",
            DeviceEnrollmentRepository.enrollmentUrlFromQrContents(
                "http://192.168.4.31:19192/client/mobile-terminal/enroll/abc",
            ),
        )
    }

    @Test
    fun deviceEnrollmentQrAcceptsJsonEnrollmentUrl() {
        assertEquals(
            "https://sm-app.example.com/client/mobile-terminal/enroll/abc",
            DeviceEnrollmentRepository.enrollmentUrlFromQrContents(
                """{"enrollment_url":"https://sm-app.example.com/client/mobile-terminal/enroll/abc"}""",
            ),
        )
    }

    @Test(expected = IllegalArgumentException::class)
    fun deviceEnrollmentQrRejectsNonHttpUrl() {
        DeviceEnrollmentRepository.enrollmentUrlFromQrContents("sm-enroll://abc")
    }

    @Test(expected = IllegalArgumentException::class)
    fun deviceEnrollmentQrRejectsPublicHttpUrl() {
        DeviceEnrollmentRepository.enrollmentUrlFromQrContents(
            "http://attacker.example.com/client/mobile-terminal/enroll/abc",
        )
    }

    @Test
    fun deviceEnrollmentLocalHttpHostClassifierMatchesPairingHosts() {
        assertTrue(DeviceEnrollmentRepository.isLocalPairingHttpHost("localhost"))
        assertTrue(DeviceEnrollmentRepository.isLocalPairingHttpHost("studio.local"))
        assertTrue(DeviceEnrollmentRepository.isLocalPairingHttpHost("127.0.0.1"))
        assertTrue(DeviceEnrollmentRepository.isLocalPairingHttpHost("10.0.0.9"))
        assertTrue(DeviceEnrollmentRepository.isLocalPairingHttpHost("172.16.1.2"))
        assertTrue(DeviceEnrollmentRepository.isLocalPairingHttpHost("192.168.4.31"))
        assertTrue(DeviceEnrollmentRepository.isLocalPairingHttpHost("fd00::1"))
        assertTrue(DeviceEnrollmentRepository.isLocalPairingHttpHost("[fc00::1]"))
        assertTrue(DeviceEnrollmentRepository.isLocalPairingHttpHost("fe80::1"))
        assertFalse(DeviceEnrollmentRepository.isLocalPairingHttpHost("8.8.8.8"))
        assertFalse(DeviceEnrollmentRepository.isLocalPairingHttpHost("example.com"))
        assertFalse(DeviceEnrollmentRepository.isLocalPairingHttpHost("fdattacker.example.com"))
        assertFalse(DeviceEnrollmentRepository.isLocalPairingHttpHost("2001:4860:4860::8888"))
    }

    private fun ticket(): MobileAttachTicketResponse {
        return MobileAttachTicketResponse(
            ticketId = "ticket-1",
            ticketSecret = "secret-1",
            deviceKeyId = "device-1",
            wsUrl = "wss://example.com/client/terminal",
            expiresAt = "2026-05-03T00:00:00Z",
        )
    }
}
