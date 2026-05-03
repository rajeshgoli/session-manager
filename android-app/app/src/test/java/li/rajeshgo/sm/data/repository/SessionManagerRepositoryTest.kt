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
}
