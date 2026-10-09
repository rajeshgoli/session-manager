package li.rajeshgo.sm.ui.queue

import kotlinx.serialization.json.Json
import kotlinx.serialization.decodeFromString
import li.rajeshgo.sm.data.model.LocalModelCard
import li.rajeshgo.sm.data.model.QueueOverview
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class LocalModelCardTest {
    @Test fun rendersSameServerWordingAsWebForEveryState() {
        val fixture = javaClass.getResource("/local-model-card.json")!!.readText()
        val cards = Json.decodeFromString<List<LocalModelCard>>(fixture)
        assertEquals(listOf("ready", "ready", "loading", "draining", "yielded", "unloaded"), cards.map { it.state })
        cards.forEach { card ->
            val lines = localModelCardLines(card)
            assertEquals("Model" to card.modelText, lines[0])
            assertEquals("Seats" to card.seatsText, lines[1])
            assertEquals("Memory headroom" to card.memoryText, lines[2])
            assertEquals(if (card.reloadText == null) 3 else 4, lines.size)
            card.reloadText?.let { assertEquals("Reload" to it, lines[3]) }
            val overview = Json { ignoreUnknownKeys = true }.decodeFromString<QueueOverview>(
                """{"local_model":${Json.encodeToString(LocalModelCard.serializer(), card)}}""")
            assertEquals(card, overview.localModel)
        }
        assertTrue(cards[4].reloadText!!.contains("held 30s"))
    }
}
