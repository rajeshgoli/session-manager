package li.rajeshgo.sm.data.repository

import li.rajeshgo.sm.data.model.MobileAttachTicketResponse
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionManagerRepositoryTest {
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
        assertFalse(repository.isRetryableMobileTerminalSocketFailure(null, "timeout"))
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
