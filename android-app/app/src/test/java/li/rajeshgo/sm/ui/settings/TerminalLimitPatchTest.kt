package li.rajeshgo.sm.ui.settings

import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.long
import kotlinx.serialization.json.jsonPrimitive
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class TerminalLimitPatchTest {
    @Test
    fun blankRestoresConfigAndUneditedFieldsAreLeftOut() {
        val patch = terminalLimitPatch(mapOf("per_user" to " 100 ", "global" to ""))["terminal_limits"]!!.jsonObject
        assertEquals(100L, patch["per_user"]!!.jsonPrimitive.long)
        assertEquals(JsonNull, patch["global"])
        assertEquals(setOf("per_user", "global"), patch.keys)
    }

    @Test
    fun outOfRangeValuesNameTheField() {
        val error = assertThrows(IllegalArgumentException::class.java) {
            terminalLimitPatch(mapOf("max_attach_seconds" to "30"))
        }
        assertEquals("Longest session (seconds): enter a whole number from 60 to 86400.", error.message)
    }
}
