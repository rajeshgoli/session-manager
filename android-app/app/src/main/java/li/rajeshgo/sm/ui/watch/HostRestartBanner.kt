package li.rajeshgo.sm.ui.watch

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import li.rajeshgo.sm.data.repository.SessionManagerRepository
import li.rajeshgo.sm.data.repository.SettingsRepository

/** One agent a host restart interrupted and nobody has decided on yet. */
data class InterruptedAgent(
    val sessionId: String,
    val name: String,
    val midTurn: Boolean,
    val failure: String?,
    val killedJobs: List<String>,
)

data class HostRestartNotice(
    val id: String,
    val restartedAt: String,
    val cause: String,
    val waiting: List<InterruptedAgent>,
)

private fun JsonObject.text(key: String): String? =
    (this[key] as? JsonPrimitive)?.takeIf { it.isString }?.content

/** The banner's model from `GET /host-restarts/latest`; null when nothing waits. */
fun parseHostRestart(response: JsonObject): HostRestartNotice? {
    val restart = response["restart"] as? JsonObject ?: return null
    val waiting = (restart["members"] as? JsonArray).orEmpty().mapNotNull { element ->
        val member = element as? JsonObject ?: return@mapNotNull null
        if (member["open"]?.jsonPrimitive?.content != "true") return@mapNotNull null
        InterruptedAgent(
            sessionId = member.text("session_id") ?: return@mapNotNull null,
            name = member.text("name").orEmpty(),
            midTurn = member["mid_turn"]?.jsonPrimitive?.content == "true",
            failure = member.text("error")?.takeIf { member.text("decision") == "failed" },
            killedJobs = (member["killed_jobs"] as? JsonArray).orEmpty()
                .mapNotNull { (it as? JsonObject)?.text("label") },
        )
    }
    if (waiting.isEmpty()) return null
    return HostRestartNotice(
        id = restart.text("id") ?: return null,
        restartedAt = restart.text("restarted_at_text").orEmpty(),
        cause = restart.text("cause_summary").orEmpty(),
        waiting = waiting,
    )
}

/**
 * "The Mac restarted at 11:49 — 7 agents were interrupted. [Restore all]"
 * (sm#2054). Reloads whenever [refreshKey] changes; hidden when nothing waits.
 */
@Composable
fun HostRestartBanner(refreshKey: Any?, onChanged: () -> Unit) {
    val context = LocalContext.current
    val settings = remember(context) { SettingsRepository(context.applicationContext) }
    val repository = remember(settings) { SessionManagerRepository(settings) }
    val scope = rememberCoroutineScope()
    var notice by remember { mutableStateOf<HostRestartNotice?>(null) }
    var busy by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf<String?>(null) }
    suspend fun load() {
        runCatching { repository.fetchLatestHostRestart(settings.serverUrl.first(), settings.accessToken.first()) }
            .onSuccess { notice = parseHostRestart(it) }
    }
    LaunchedEffect(refreshKey) { load() }
    val current = notice ?: return
    fun act(work: suspend (String, String) -> Result<JsonObject>, done: String) {
        scope.launch {
            busy = true
            val result = work(settings.serverUrl.first(), settings.accessToken.first())
            message = result.fold(
                onSuccess = { body ->
                    val failed = (body["results"] as? JsonArray).orEmpty()
                        .mapNotNull { it as? JsonObject }
                        .filter { it.text("outcome") != "restored" }
                    if (failed.isEmpty()) done else "${failed.size} not restored: ${failed.first().text("error")}"
                },
                onFailure = { it.message ?: "Couldn't reach sm" },
            )
            load()
            busy = false
            onChanged()
        }
    }
    val count = current.waiting.size
    Surface(shape = RoundedCornerShape(18.dp), color = MaterialTheme.colorScheme.secondaryContainer) {
        Column(Modifier.fillMaxWidth().padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(
                "The Mac restarted at ${current.restartedAt} — $count agent${if (count == 1) " was" else "s were"} interrupted.",
                style = MaterialTheme.typography.titleSmall,
            )
            Text(current.cause, style = MaterialTheme.typography.bodySmall)
            Button(enabled = !busy, onClick = {
                act({ url, token -> repository.restoreHostRestart(url, token, current.id, buildJsonObject {}) }, "Restored $count agents")
            }) { Text("Restore all") }
            current.waiting.forEach { agent ->
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Column(Modifier.weight(1f)) {
                        Text(agent.name, style = MaterialTheme.typography.bodyMedium)
                        val detail = listOfNotNull(
                            if (agent.midTurn) "mid-turn" else "idle",
                            agent.killedJobs.takeIf { it.isNotEmpty() }?.let { "killed: ${it.joinToString()}" },
                            agent.failure?.let { "restore failed: $it" },
                        ).joinToString(" · ")
                        Text(detail, style = MaterialTheme.typography.bodySmall)
                    }
                    TextButton(enabled = !busy, onClick = {
                        act({ url, token ->
                            repository.restoreHostRestart(url, token, current.id, buildJsonObject {
                                put("session_ids", JsonArray(listOf(JsonPrimitive(agent.sessionId))))
                            })
                        }, "Restored ${agent.name}")
                    }) { Text("Restore") }
                    TextButton(enabled = !busy, onClick = {
                        act({ url, token -> repository.leaveHostRestartMember(url, token, current.id, agent.sessionId) }, "Left ${agent.name} retired")
                    }) { Text("Leave retired") }
                }
            }
            message?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
        }
    }
}
