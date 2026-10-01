package li.rajeshgo.sm.ui.inbox

import kotlinx.serialization.json.Json
import li.rajeshgo.sm.data.model.InboxRow
import li.rajeshgo.sm.data.model.InboxThread
import li.rajeshgo.sm.data.model.DocAskTarget
import li.rajeshgo.sm.data.model.InboxReplyOption
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ThreadModelsTest {
    private val json = Json { ignoreUnknownKeys = true }

    /** `GET /inbox/thread/{key}?format=json`, trimmed: a question, your reply, a turn and a doc event. */
    private val thread = json.decodeFromString(
        InboxThread.serializer(),
        """{"thread_key":"ticket:rajeshgoli/session-manager#1801","title":"#1801 Fit and finish 14/14",
           "status":"live","repo":"rajeshgoli/session-manager","can_send":true,
           "reply_to":{"id":"c26eb47e","name":"sm-1801","restores":false,"retired_at":null},
           "reply_options":[],"review_asks":[],
           "items":[
             {"kind":"message","id":"msg-1","at":"2026-09-30T10:00:00Z","title":"Keep it?","markdown":"# Keep it?\n\nx",
              "state":"needs you","needs_you":true,"html":"<div></div>",
              "sender":{"id":"a1b2c3d4","name":"sm-1800","status":"ended"}},
             {"kind":"owner","id":"sub-1","at":"2026-09-30T10:05:00Z","body":"Yes.",
              "quotes":[{"quote":"Keep it?","body":""}],"to":null,"html":"","sender":null},
             {"type":"turn","kind":"turn","at":"2026-09-30T10:06:00Z","finished":true,"markdown":"Done.","html":"",
              "sender":{"id":"c26eb47e","name":"sm-1801","status":"live"}},
             {"kind":"event","at":"2026-09-30T10:07:00Z","text":"Published: Memo","link":"/docs/session-manager/memo.html",
              "type":"doc_revision","html":"","sender":{"id":"c26eb47e","name":"sm-1801","status":"live"}}
           ]}""",
    )

    @Test fun threadJsonParses() {
        assertEquals(4, thread.items.size)
        assertEquals("Yes.", thread.items[1].body)
        assertEquals("Keep it?", thread.items[1].quotes.single().quote)
        assertEquals("/docs/session-manager/memo.html", thread.items[3].link)
        assertEquals("sm-1801", thread.replyTo?.name)
    }

    @Test fun askTargetAndWorkThreadReplyOptionsParse() {
        val ask = json.decodeFromString(DocAskTarget.serializer(),
            """{"author":{"id":"s1","name":"sm-1821","state":"ended","live":false,"restorable":true,"context_tokens":380000},"default":"reader","reader":null,"thread_key":"ticket:o/r#1821","first_published_at":"2026-09-30T10:00:00Z"}""")
        assertEquals("ticket:o/r#1821", ask.threadKey)
        assertEquals(380000L, ask.author?.contextTokens)
        assertEquals("2026-09-30T10:00:00Z", ask.firstPublishedAt)
        val work = json.decodeFromString(InboxThread.serializer(),
            """{"thread_key":"ticket:o/r#1821","can_send":true,"reply_to":{"id":"s2","name":"sm-1821-2","restores":true,"retired_at":"2026-09-30T10:00:00Z"},"reply_options":[{"id":"s1","name":"sm-1821","status":"ended","can_send":false},{"id":"s2","name":"sm-1821-2","status":"ended","can_send":true,"recipient_id":"s2","recipient_name":"sm-1821-2","restores":true}],"items":[{"kind":"event","type":"doc_revision","doc_id":"abc","pr":1827,"sha":"1234567890abcdef","review_state":"requested","at":"2026-09-30T10:00:00Z","text":"Published: Memo","link":"/docs/session-manager/memo.html?version=1234567890ab"}]}""")
        assertEquals(true, work.replyTo?.restores)
        assertEquals("s2", work.replyOptions.last().recipientId)
        assertEquals("abc", work.items.single().docId)
        assertEquals("requested", work.items.single().reviewState)
        val earlier = work.items.single().copy(at = "2026-09-29T10:00:00Z", id = "old")
        val visible = docAskThread(work.copy(items = listOf(earlier) + work.items), ask.firstPublishedAt)
        assertEquals(listOf("abc"), visible.items.map { it.docId })
        val fractional = docAskThread(work, "2026-09-30T10:00:00.789Z")
        assertEquals(1, fractional.items.size)
    }

    @Test fun answeredClearsTheAgentsThatAsked() {
        assertEquals(listOf("a1b2c3d4"), threadAskers(thread))
        assertEquals(emptyList<String>(), threadAskers(thread.copy(items = thread.items.drop(1))))
    }

    @Test fun terminalGoesToTheReplyTargetElseTheNewestSender() {
        assertEquals("c26eb47e" to "sm-1801", threadTerminalAgent(thread))
        assertEquals("c26eb47e" to "sm-1801", threadTerminalAgent(thread.copy(replyTo = null)))
        assertNull(threadTerminalAgent(thread.copy(replyTo = null, items = emptyList())))
    }

    @Test fun threadKeysNameTheirGitHubItem() {
        assertEquals("#1801 ↗" to "https://github.com/rajeshgoli/session-manager/issues/1801", threadWorkLink(thread.threadKey))
        assertEquals("PR #12 ↗" to "https://github.com/o/r/pull/12", threadWorkLink("pr:o/r#12"))
        assertNull(threadWorkLink("agent:c26eb47e"))
        assertNull(threadWorkLink("docpath:o/r/docs/memo.html"))
    }

    @Test fun defaultReplyNamesTheLiveSuccessorRatherThanTheEndedForwarder() {
        val options = listOf(
            InboxReplyOption(id = "ended", name = "sm-old", status = "ended", canSend = true, recipientId = "live"),
            InboxReplyOption(id = "live", name = "sm-new", status = "live", canSend = true, recipientId = "live"),
        )
        assertEquals("live", defaultReplyOptionId(thread.copy(replyOptions = options)))
        assertEquals("ended", defaultReplyOptionId(thread.copy(replyOptions = options.take(1))))
        assertEquals("sm-old → sm-new", replyOptionLabel(options.first().copy(recipientName = "sm-new")))
    }

    @Test fun notificationPathsOpenTheirThread() {
        assertEquals(
            ThreadTarget("ticket:o/r#1782", null, "t"),
            threadTargetForPath("/inbox/thread/ticket%3Ao%2Fr%231782?at=msg-1", "t"),
        )
        assertEquals(ThreadTarget(null, "eng00001", "t"), threadTargetForPath("/inbox/agent/eng00001?at=msg-1", "t"))
        assertNull(threadTargetForPath("/messages/msg-1", "t"))
        assertNull(threadTargetForPath("/docs/session-manager/memo.html", "t"))
    }

    @Test fun docRowsOpenTheDocAndOthersTheirThread() {
        val doc = InboxRow(threadKey = "docpath:o/r/docs/m.html", kind = "doc", url = "/inbox/thread/x", docUrl = "/docs/r/docs/m.html")
        assertNull(inboxThreadTarget(doc))
        assertEquals("/docs/r/docs/m.html", inboxReaderPage(doc).path)
        val ticket = InboxRow(threadKey = "ticket:o/r#7", kind = "ticket", title = "#7 X", sessionId = "s1")
        assertEquals(ThreadTarget("ticket:o/r#7", "s1", "#7 X"), inboxThreadTarget(ticket))
    }

    /** The client does not follow redirects; an agent's thread is read from the redirect's target. */
    @Test fun agentThreadRedirectNamesItsKey() {
        assertEquals("ticket:o/r#1858", li.rajeshgo.sm.data.repository.threadKeyFromLocation("/inbox/thread/ticket%3Ao%2Fr%231858?format=json"))
        assertEquals("agent:df9fec5a", li.rajeshgo.sm.data.repository.threadKeyFromLocation("/inbox/thread/agent%3Adf9fec5a"))
        assertNull(li.rajeshgo.sm.data.repository.threadKeyFromLocation("/login"))
        assertNull(li.rajeshgo.sm.data.repository.threadKeyFromLocation(null))
    }
}
