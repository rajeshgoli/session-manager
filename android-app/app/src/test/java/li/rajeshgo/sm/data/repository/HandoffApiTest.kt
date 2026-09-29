package li.rajeshgo.sm.data.repository

import com.jakewharton.retrofit2.converter.kotlinx.serialization.asConverterFactory
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.*
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.remote.ApiService
import li.rajeshgo.sm.ui.handoff.handoffSummary
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.Assert.*
import org.junit.Test

class HandoffApiTest {
    private val json = Json { ignoreUnknownKeys = true }
    private val policy = """{"enabled":true,"threshold_percent":42.5,"source":"override","has_gauge":true,"display":"handoff overdue","state":"asked","successor":null,"predecessor":{"id":"old","name":"agent"}}"""
    private val defaults = """{"providers":{"claude":true,"codex-fork":false,"codex-app":false},"threshold_percent":35,"ask_on_codex_review":true,"ask_on_doc_review":true,"review_floor_percent":20,"reminder_percent":50,"updated_at":"now"}"""

    @Test
    fun ownerRoutesPreservePartialBodies() = runBlocking {
        val seen = mutableListOf<String>()
        val client = okhttp3.OkHttpClient.Builder().addInterceptor { chain ->
            val request = chain.request()
            val buffer = okio.Buffer()
            request.body?.writeTo(buffer)
            seen += "${request.method} ${request.url.encodedPath} ${buffer.readUtf8()}"
            assertNull(request.header("X-SM-Session"))
            okhttp3.Response.Builder().request(request).protocol(okhttp3.Protocol.HTTP_1_1)
                .code(200).message("OK")
                .body((if (request.url.encodedPath.endsWith("handoff-defaults")) defaults else policy).toResponseBody("application/json".toMediaType())).build()
        }.build()
        val api = retrofit2.Retrofit.Builder().baseUrl("https://example.com/").client(client)
            .addConverterFactory(json.asConverterFactory("application/json".toMediaType()))
            .build().create(ApiService::class.java)
        assertEquals("handoff overdue", api.getHandoffPolicy("agent").display)
        listOf(
            """{"enabled":false}""", """{"threshold_percent":42.5}""",
            """{"use_default":true}""", """{"ask_now":true}""",
        ).forEach { api.setHandoffPolicy("agent", json.parseToJsonElement(it).jsonObject) }
        assertEquals(35.0, api.getHandoffDefaults().thresholdPercent, 0.0)
        listOf(
            """{"providers":{"codex-app":true}}""", """{"threshold_percent":100}""",
            """{"review_floor_percent":0}""", """{"reminder_percent":1}""",
            """{"ask_on_codex_review":false}""", """{"ask_on_doc_review":false}""",
        ).forEach { api.setHandoffDefaults(json.parseToJsonElement(it).jsonObject) }
        assertEquals(listOf(
            "GET /sessions/agent/handoff-policy ",
            """PUT /sessions/agent/handoff-policy {"enabled":false}""",
            """PUT /sessions/agent/handoff-policy {"threshold_percent":42.5}""",
            """PUT /sessions/agent/handoff-policy {"use_default":true}""",
            """PUT /sessions/agent/handoff-policy {"ask_now":true}""",
            "GET /handoff-defaults ",
            """PUT /handoff-defaults {"providers":{"codex-app":true}}""",
            """PUT /handoff-defaults {"threshold_percent":100}""",
            """PUT /handoff-defaults {"review_floor_percent":0}""",
            """PUT /handoff-defaults {"reminder_percent":1}""",
            """PUT /handoff-defaults {"ask_on_codex_review":false}""",
            """PUT /handoff-defaults {"ask_on_doc_review":false}""",
        ), seen)
    }

    @Test
    fun sessionDecodesGaugeAndUsesServerDisplayAcrossStates() {
        val base = """{"id":"agent","name":"agent","working_dir":"/tmp","status":"running","created_at":"now","last_activity":"now","tmux_session":"agent"}"""
        val old = json.decodeFromString<ClientSession>(base)
        assertNull(old.handoff)
        assertNull(handoffSummary(old))
        val session = json.decodeFromString<ClientSession>(base.dropLast(1) + """, "context_percent":51.5,"handoff":$policy}""")
        assertEquals(51.5, session.contextPercent!!, 0.0)
        assertEquals("old", session.handoff!!.predecessor!!.id)
        listOf("hands off at 42.5%", "handoff off", "asked 14:02", "handoff overdue", "handing off", "handoff failed", "→ agent-h2").forEach { display ->
            assertEquals("Context 52% · $display", handoffSummary(session.copy(handoff = session.handoff.copy(display = display))))
        }
        assertEquals("handoff overdue", handoffSummary(session.copy(contextPercent = null)))
    }
}
