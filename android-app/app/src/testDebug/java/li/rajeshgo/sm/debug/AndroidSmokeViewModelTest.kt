package li.rajeshgo.sm.debug

import androidx.lifecycle.ViewModelStore
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import kotlinx.coroutines.test.setMain
import org.junit.Assert.*
import org.junit.Test

@OptIn(kotlinx.coroutines.ExperimentalCoroutinesApi::class)
class AndroidSmokeViewModelTest {
    @Test
    fun recreatedActivityObservesTheSameEnrollmentEvenAfterCompletion() = runTest {
        Dispatchers.setMain(StandardTestDispatcher(testScheduler))
        val store = ViewModelStore()
        try {
            val model = AndroidSmokeViewModel()
            store.put("smoke", model)
            val response = CompletableDeferred<Unit>()
            var requests = 0
            val firstRun = model.start { requests++; response.await() }
            val firstActivity = launch { firstRun.join() }
            runCurrent()
            // Only the Activity's scope is cancelled on configuration change.
            firstActivity.cancel()
            val recreated = store.get("smoke") as AndroidSmokeViewModel
            val nextRun = recreated.start { requests++ }
            runCurrent()
            assertSame(firstRun, nextRun)
            assertTrue(firstRun.isActive)
            assertEquals(1, requests)
            response.complete(Unit)
            runCurrent()
            assertTrue(firstRun.isCompleted)
            assertSame(firstRun, recreated.start { requests++ })
            runCurrent()
            assertEquals(1, requests)
        } finally {
            store.clear()
            Dispatchers.resetMain()
        }
    }

    @Test
    fun finishingActivityCancelsRetainedRun() = runTest {
        Dispatchers.setMain(StandardTestDispatcher(testScheduler))
        val store = ViewModelStore()
        try {
            val model = AndroidSmokeViewModel()
            store.put("smoke", model)
            val pending = CompletableDeferred<Unit>()
            val run = model.start { pending.await() }
            runCurrent()
            store.clear()
            runCurrent()
            assertTrue(run.isCancelled)
        } finally {
            store.clear()
            Dispatchers.resetMain()
        }
    }
}
