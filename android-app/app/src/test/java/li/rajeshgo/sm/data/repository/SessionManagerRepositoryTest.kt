package li.rajeshgo.sm.data.repository

import org.junit.Assert.assertEquals
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
}
