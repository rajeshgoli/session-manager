package li.rajeshgo.sm.ui.handoff

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.*
import li.rajeshgo.sm.data.model.HandoffDefaults
import li.rajeshgo.sm.data.model.HandoffPolicy
import org.junit.Assert.*
import org.junit.Test

@OptIn(kotlinx.coroutines.ExperimentalCoroutinesApi::class)
class HandoffViewModelTest {
    private val policy = HandoffPolicy(true, 35.0, "default", true, "hands off at 35%")
    private val defaults = HandoffDefaults(mapOf("claude" to true), 35.0, true, true, 20.0, 50.0)

    @Test
    fun agentActionsUsePartialUpdatesAndApplyServerResponse() = runTest {
        val patches = mutableListOf<JsonObject>()
        val asked = policy.copy(display = "asked 14:02", state = "asked")
        val model = HandoffViewModel(this, { HandoffUiState(policy = policy) }, { patch ->
            patches += patch
            HandoffUiState(policy = asked)
        })
        model.refresh(); runCurrent()
        listOf(
            buildJsonObject { put("enabled", false) },
            buildJsonObject { put("threshold_percent", 42.5) },
            buildJsonObject { put("use_default", true) },
            buildJsonObject { put("ask_now", true) },
        ).forEach { patch ->
            model.update(patch); runCurrent()
            assertEquals(patch, patches.last())
            assertEquals(asked, model.state.value.policy)
            assertFalse(model.state.value.saving)
        }
        assertEquals(4, patches.size)
    }

    @Test
    fun defaultChangesWaitForServerAndRejectDuplicateTaps() = runTest {
        val response = CompletableDeferred<HandoffUiState>()
        var writes = 0
        val model = HandoffViewModel(this, { HandoffUiState(defaults = defaults) }, {
            writes++
            response.await()
        })
        model.refresh(); runCurrent()
        val patch = buildJsonObject { put("review_floor_percent", 0) }
        model.update(patch); model.update(patch); runCurrent()
        assertTrue(model.state.value.saving)
        assertEquals(defaults, model.state.value.defaults)
        assertEquals(1, writes)
        model.refresh(); runCurrent()
        response.complete(HandoffUiState(defaults = defaults.copy(reviewFloorPercent = 0.0)))
        runCurrent()
        assertEquals(0.0, model.state.value.defaults!!.reviewFloorPercent, 0.0)
        assertFalse(model.state.value.saving)
    }

    @Test
    fun failedSaveKeepsConfirmedValuesAndCanBeRetried() = runTest {
        var fail = true
        val model = HandoffViewModel(this, { HandoffUiState(policy = policy) }, {
            if (fail) error("Server unavailable")
            HandoffUiState(policy = policy.copy(enabled = false, display = "handoff off"))
        })
        model.refresh(); runCurrent()
        val patch = buildJsonObject { put("enabled", false) }
        model.update(patch); runCurrent()
        assertEquals(policy, model.state.value.policy)
        assertEquals("Server unavailable", model.state.value.error)
        assertFalse(model.state.value.saving)
        fail = false
        model.update(patch); runCurrent()
        assertFalse(model.state.value.policy!!.enabled)
        assertNull(model.state.value.error)
    }

    @Test
    fun readStartedBeforeWriteCannotUndoSuccessfulChange() = runTest {
        val staleRead = CompletableDeferred<HandoffUiState>()
        var reads = 0
        val model = HandoffViewModel(this, {
            if (++reads == 1) HandoffUiState(policy = policy)
            else withContext(NonCancellable) { staleRead.await() }
        }, { HandoffUiState(policy = policy.copy(thresholdPercent = 45.0)) })
        model.refresh(); runCurrent()
        model.refresh(); runCurrent()
        model.update(buildJsonObject { put("threshold_percent", 45) }); runCurrent()
        staleRead.complete(HandoffUiState(policy = policy)); runCurrent()
        assertEquals(45.0, model.state.value.policy!!.thresholdPercent, 0.0)
    }

    @Test
    fun loadFailureDoesNotEnableWrites() = runTest {
        var writes = 0
        val model = HandoffViewModel(this, { error("Forbidden") }, { writes++; HandoffUiState() })
        model.refresh(); runCurrent()
        model.update(buildJsonObject { put("ask_now", true) }); runCurrent()
        assertEquals(0, writes)
        assertEquals("Forbidden", model.state.value.error)
        assertFalse(model.state.value.loading)
    }

    @Test
    fun successfulRefreshClearsReadFailure() = runTest {
        var fail = true
        val model = HandoffViewModel(this, {
            if (fail) error("Offline")
            HandoffUiState(defaults = defaults)
        }, { HandoffUiState() })
        model.refresh(); runCurrent()
        assertEquals("Offline", model.state.value.error)
        fail = false
        model.refresh(); runCurrent()
        assertNull(model.state.value.error)
        assertEquals(defaults, model.state.value.defaults)
    }

    @Test
    fun pickersPreserveOffStepValuesAndValidBoundaries() {
        assertEquals((5..95 step 5).map(Int::toDouble) + 100.0, thresholdOptions(100.0))
        assertTrue(thresholdOptions(42.5).contains(42.5))
        assertTrue(thresholdOptions(1.0).contains(1.0))
        assertEquals(19, thresholdOptions(35.0).size)
        assertEquals(0.0, defaultPercentOptions(20.0, true).first(), 0.0)
        assertEquals(1.0, defaultPercentOptions(35.0, false).first(), 0.0)
        assertEquals(100.0, defaultPercentOptions(50.0, false).last(), 0.0)
    }
}
