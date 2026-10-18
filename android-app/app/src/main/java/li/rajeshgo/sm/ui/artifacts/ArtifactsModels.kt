package li.rajeshgo.sm.ui.artifacts

import java.util.Locale
import kotlinx.serialization.json.*

private const val GB = 1_000_000_000L
fun measuredArtifactBytes(a: JsonObject): Long? =
    (a[if (a.text("state") == "frozen") "frozen_bytes" else "disk_bytes"] as? JsonPrimitive)?.longOrNull

fun storageSize(bytes: Long?): String {
    if (bytes == null) return "Not measured"
    if (bytes == 0L) return "0 MB"
    if (bytes < 1_000_000) return "<1 MB"
    val (unit, divisor) = when {
        bytes >= 1_000_000_000_000L -> "TB" to 1e12
        bytes >= GB -> "GB" to 1e9
        else -> "MB" to 1e6
    }
    val value = bytes / divisor
    return (if (value < 10) String.format(Locale.US, "%.1f", value) else String.format(Locale.US, "%.0f", value)) + " $unit"
}
fun diskName(f: JsonObject): String = f.text("path").let {
    if (it.startsWith("/Volumes/")) it.removePrefix("/Volumes/").substringBefore('/') else "Mac storage"
}
fun artifactNeedsDecision(a: JsonObject): Boolean =
    (a["needs_you"] as? JsonObject)?.get("reasons")?.jsonArray?.any { it.jsonPrimitive.content == "would_delete" } == true

fun artifactNeedsAgent(a: JsonObject): Boolean =
    (a["needs_you"] as? JsonObject)?.get("reasons")?.jsonArray?.any { it.jsonPrimitive.content in listOf("operation_failed", "consumer_unverifiable", "vanished") } == true

fun artifactInventory(rows: List<JsonObject>, query: String = ""): List<JsonObject> = rows
    .filter { (it.text("state") != "deleted" || artifactNeedsAgent(it)) && (query.isBlank() || listOf("name", "note", "epic", "repo").any { key -> it.text(key).contains(query, ignoreCase = true) }) }
    .sortedWith(compareByDescending<JsonObject> { measuredArtifactBytes(it) ?: -1L }.thenBy { it.text("name") })
fun largestArtifacts(rows: List<JsonObject>): List<JsonObject> = artifactInventory(rows)
    .filter { !artifactNeedsDecision(it) && !artifactNeedsAgent(it) && (measuredArtifactBytes(it) ?: -1L) >= GB }.take(5)
fun artifactSummary(a: JsonObject): String = when {
    a.text("investigating").isNotEmpty() -> "Agent investigating"
    artifactNeedsAgent(a) -> if (a.text("state") == "deleted") "Folder missing · agent follow-up needed" else "Storage issue · agent follow-up needed"
    artifactNeedsDecision(a) -> "Cleanup proposed · review before deleting"
    a.text("state") == "unrecorded" -> "Needs an agent's record"
    a.text("state") == "frozen" -> "Compressed · restored when needed"
    a.flag("hot") -> "In recent use"
    else -> "Saved work"
}
fun backupSummary(backup: JsonObject?): String = when {
    backup == null -> "Backup status unavailable"
    !backup.flag("enabled") -> "Backup is off"
    backup.text("confirmed_at").isNotEmpty() -> "Backup confirmed ${backup.text("confirmed_at").take(10)}"
    else -> "No confirmed backup yet"
}

/** The remote disk's latest self-update of its sm (#2264), or null. */
fun backupUpdate(backup: JsonObject?): String? {
    val update = backup?.get("update") as? JsonObject ?: return null
    val on = update.text("at").take(10).let { if (it.isEmpty()) "" else " on $it" }
    return if (update.flag("ok")) "Remote disk: updated to ${update.text("to")}$on"
    else "Remote disk: update to ${update.text("to")} failed$on: ${update.text("error").ifEmpty { "unknown error" }}"
}

fun artifactReport(a: JsonObject): String? = (a["report"] as? JsonObject)?.text("detail_json")?.let { text ->
    runCatching { Json.parseToJsonElement(text).jsonObject.text("note").ifBlank { text } }.getOrDefault(text)
}
