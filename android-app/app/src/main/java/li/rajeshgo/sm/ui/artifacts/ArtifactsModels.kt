package li.rajeshgo.sm.ui.artifacts

import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
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

data class BackupNight(val date: String, val ok: Boolean?, val mark: String, val detail: String)
data class BackupDisk(val used: Long, val total: Long, val line: String, val holding: String, val pruned: String?, val health: String, val failing: Boolean)
data class BackupCard(val nights: List<BackupNight>, val lastNight: String, val check: String, val checkOk: Boolean,
    val disk: BackupDisk?, val rates: String, val inForce: String)

private fun JsonObject.decimal(key: String): Double? = (get(key) as? JsonPrimitive)?.doubleOrNull
private fun JsonObject.child(key: String): JsonObject? = get(key) as? JsonObject
fun backupRate(bps: Double?): String = if (bps == null || !bps.isFinite()) "unknown" else "${Math.round(bps * 8 / 1e6)} Mbit/s"
private fun backupDuration(seconds: Long): String = if (seconds < 60) "${seconds}s" else "${(seconds + 59) / 60} min"
private fun localTime(at: String): String = runCatching {
    DateTimeFormatter.ofPattern("MMM d, h:mm a", Locale.US).format(Instant.parse(at).atZone(ZoneId.systemDefault()))
}.getOrDefault(at)

/** The Backup card (#2276), from the remote disk's acknowledgements; null when backup is off. */
fun backupCard(backup: JsonObject?): BackupCard? {
    if (backup == null || !backup.flag("enabled")) return null
    val card = backup.child("card")
    val nights = card?.rows("nights").orEmpty().map { n ->
        val ok = (n["ok"] as? JsonPrimitive)?.booleanOrNull
        val date = n.text("date")
        BackupNight(date, ok, when (ok) { true -> "✓"; false -> "✗"; null -> "·" },
            if (ok == true) "$date: confirmed" else "$date: ${n.text("error").ifEmpty { "not confirmed" }}")
    }
    val last = card?.child("last_night")
    val check = card?.child("weekly_check")
    val disk = card?.child("disk")?.let { d ->
        val total = d.number("total_bytes")
        val free = d.number("free_bytes")
        val used = (total - free).coerceAtLeast(0)
        BackupDisk(used, total, "${storageSize(used)} used · ${storageSize(free)} free of ${storageSize(total)}",
            "Holding ${storageSize(d.number("holding_bytes"))}" + d.text("holding_oldest").let { if (it.isEmpty()) "" else " · oldest $it" },
            d.text("pruned_on").takeIf { it.isNotEmpty() }?.let { "Holding shortened for space on $it" },
            when (d.text("health")) { "verified" -> "Verified"; "failing" -> "Failing"; else -> "Not reported" },
            d.text("health") == "failing")
    }
    return BackupCard(
        nights = nights,
        lastNight = last?.let { "Last backup: ${storageSize(it.number("bytes"))} in ${backupDuration(it.number("seconds"))}, confirmed ${localTime(it.text("confirmed_at"))}" }
            ?: "No confirmed backup yet",
        check = check?.let { "Weekly check ${it.text("date")}: ${it.number("files_checked")} files, " + if (it.flag("ok")) "all ok" else "${it.number("bad")} bad" }
            ?: "Weekly check: not run yet",
        checkOk = check == null || check.flag("ok"),
        disk = disk,
        rates = listOf("Download" to "fetch_rate", "Disk" to "apply_rate", "Restore" to "probe_rate")
            .joinToString(" · ") { (name, key) -> "$name ${backupRate(backup.decimal(key))}" },
        inForce = backupRate(backup.decimal("effective_rate")),
    )
}

fun artifactReport(a: JsonObject): String? = (a["report"] as? JsonObject)?.text("detail_json")?.let { text ->
    runCatching { Json.parseToJsonElement(text).jsonObject.text("note").ifBlank { text } }.getOrDefault(text)
}
