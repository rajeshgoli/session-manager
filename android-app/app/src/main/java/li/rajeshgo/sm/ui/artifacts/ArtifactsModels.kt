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
    artifactOp(a) != null -> artifactOp(a)!!
    artifactNeedsAgent(a) -> if (a.text("state") == "deleted") "Folder missing · agent follow-up needed" else "Storage issue · agent follow-up needed"
    artifactNeedsDecision(a) -> "Cleanup proposed · review before deleting"
    a.text("state") == "unrecorded" -> "Needs an agent's record"
    a.text("state") == "frozen" -> "Frozen" + (placementDisk(a)?.let { " on $it" } ?: "") +
        ((a["saved_bytes"] as? JsonPrimitive)?.longOrNull?.takeIf { it > 0 }?.let { " · saved ${storageSize(it)}" } ?: "") + " · thawed when needed"
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

data class BackupNight(val date: String, val ok: Boolean, val mark: String, val detail: String)
data class BackupDisk(val used: Long, val total: Long, val line: String, val holding: String, val pruned: String?, val health: String, val failing: Boolean)
data class BackupCard(val empty: Boolean, val attention: String?, val nights: List<BackupNight>, val lastNight: String?, val check: String?,
    val checkOk: Boolean, val disk: BackupDisk?, val rates: String?)

private fun JsonObject.decimal(key: String): Double? = (get(key) as? JsonPrimitive)?.doubleOrNull
private fun JsonObject.child(key: String): JsonObject? = get(key) as? JsonObject
fun backupRate(bps: Double?): String = if (bps == null || !bps.isFinite()) "unknown" else "${Math.round(bps * 8 / 1e6)} Mbit/s"
private fun backupDuration(seconds: Long): String = if (seconds < 60) "${seconds}s" else "${(seconds + 59) / 60} min"
private fun localTime(at: String): String = runCatching {
    DateTimeFormatter.ofPattern("MMM d, h:mm a", Locale.US).format(Instant.parse(at).atZone(ZoneId.systemDefault()))
}.getOrDefault(at)

/** The Backup card (#2276), from the remote disk's acknowledgements; null when backup is off.
 *  Each line appears only when it has something real to report (#2291). */
fun backupCard(backup: JsonObject?): BackupCard? {
    if (backup == null || !backup.flag("enabled")) return null
    val card = backup.child("card")
    val nights = card?.rows("nights").orEmpty().mapNotNull { n ->
        val ok = (n["ok"] as? JsonPrimitive)?.booleanOrNull ?: return@mapNotNull null
        val date = n.text("date")
        BackupNight(date, ok, if (ok) "✓" else "✗", if (ok) "$date: confirmed" else "$date: ${n.text("error").ifEmpty { "not confirmed" }}")
    }
    val last = card?.child("last_night")
    val check = card?.child("weekly_check")
    val measured = card?.child("measured")
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
    val latest = nights.lastOrNull()
    val rates = listOf(Triple("Download", "fetch", "fetch_rate"), Triple("Disk", "apply", "apply_rate"), Triple("Restore", "probe", "probe_rate"))
        .filter { measured?.flag(it.second) == true }.map { (name, _, key) -> "$name ${backupRate(backup.decimal(key))}" }
    return BackupCard(
        empty = nights.isEmpty() && last == null && check == null && disk == null,
        attention = when {
            disk?.failing == true -> "Backup drive is failing. Replace it, then reseed with sm backup seed."
            check != null && !check.flag("ok") -> "Backup check found ${check.number("bad")} damaged files on the remote disk"
            latest?.ok == false -> "Last night's backup failed: ${latest.detail.removePrefix("${latest.date}: ")}"
            else -> null
        },
        nights = nights,
        lastNight = last?.let { "Last backup: ${storageSize(it.number("bytes"))} in ${backupDuration(it.number("seconds"))}, confirmed ${localTime(it.text("confirmed_at"))}" },
        check = check?.let { "Weekly check ${it.text("date")}: ${it.number("files_checked")} files, " + if (it.flag("ok")) "all ok" else "${it.number("bad")} bad" },
        checkOk = check == null || check.flag("ok"),
        disk = disk,
        rates = if (rates.isEmpty()) null else rates.joinToString(" · ") + " · in force ${backupRate(backup.decimal("effective_rate"))}",
    )
}

fun artifactReport(a: JsonObject): String? = (a["report"] as? JsonObject)?.text("detail_json")?.let { text ->
    runCatching { Json.parseToJsonElement(text).jsonObject.text("note").ifBlank { text } }.getOrDefault(text)
}

// ---- freeze, thaw and move (sm#2325); wording matches the web page ----------
data class OpKind(val icon: String, val now: String, val past: String)
fun opKind(kind: String): OpKind = when (kind) {
    "freezing" -> OpKind("freeze", "Freezing", "Froze")
    "thawing" -> OpKind("thaw", "Thawing", "Thawed")
    "moving" -> OpKind("move", "Moving to cold disk", "Moved to cold disk")
    else -> OpKind("move", kind, kind)
}
val OP_STEPS = listOf("Queued", "Copying", "Finishing", "Done")
/** The step an operation has reached: queued 0, copying 1, finishing 2, done 3, failed -1. */
fun opStep(op: JsonObject): Int = when (op.text("phase")) {
    "copying" -> 1; "prepared", "cleanup" -> 2; "done" -> 3; "failed" -> -1; else -> 0
}
/** "45 s", "12 min", "1 h 20 min". */
fun span(seconds: Double?): String? {
    if (seconds == null || !seconds.isFinite()) return null
    if (seconds < 60) return "${Math.round(seconds).coerceAtLeast(1)} s"
    val minutes = Math.round(seconds / 60)
    return if (minutes < 60) "$minutes min" else "${minutes / 60} h" + if (minutes % 60 != 0L) " ${minutes % 60} min" else ""
}
private fun JsonObject.long(key: String): Long? = (get(key) as? JsonPrimitive)?.doubleOrNull?.toLong()
/** The line under an operation: where it is in line and when it will finish. */
fun opStatus(op: JsonObject): String {
    val left = span(op.decimal("remaining_seconds"))
    val error = op.text("error")
    return when (op.text("state")) {
        "done" -> "Done" + (span(op.decimal("seconds"))?.let { " · took $it" } ?: "")
        "failed" -> "Failed: ${error.ifEmpty { "no reason recorded" }}"
        "running" -> {
            val behind = op.number("behind").let { if (it > 0) " · $it waiting behind it" else "" }
            if (op.flag("over_estimate")) "Running now$behind · taking longer than recent ${opKind(op.text("kind")).now.lowercase()}"
            else if ((op.decimal("remaining_seconds") ?: 1.0) < 1) "Running now$behind · should finish any moment"
            else "Running now$behind" + (left?.let { " · about $it left" } ?: "")
        }
        else -> when {
            error.contains("capacity") -> "Waiting for space on the cold disk above its floor"
            error.isNotEmpty() -> "Waiting: $error"
            else -> {
                val ahead = op.number("ahead")
                val starts = span(op.decimal("starts_in_seconds"))
                "Waiting · " + (if (ahead > 0) "$ahead ahead" else "next in line") +
                    (if (starts != null && ahead > 0) " · starts in about $starts" else "") + (left?.let { " · done in about $it" } ?: "")
            }
        }
    }
}
fun opTook(op: JsonObject): String? = span(op.decimal("seconds"))
/** How far an estimated copy has run, 0–1, or null without an estimate. */
fun opProgress(op: JsonObject): Float? {
    val expected = op.decimal("expected_seconds") ?: return null
    val elapsed = op.decimal("elapsed_seconds") ?: return null
    return if (op.text("state") != "running" || expected <= 0) null else (elapsed / expected).toFloat().coerceAtMost(0.97f)
}
/** Who asked for it: a session name with its id, the owner, or sm. */
fun actorLabel(op: JsonObject): String = op.text("actor_name").takeIf { it.isNotEmpty() && it != op.text("actor") } ?: op.text("actor")
fun actorId(op: JsonObject): String? = op.text("actor").takeIf { actorLabel(op) != it && Regex("^[0-9a-f]{8}$").matches(it) }
fun clock(at: String): String = if (at.isEmpty()) "" else localTime(at)
fun day(at: String): String = runCatching {
    DateTimeFormatter.ofPattern("MMM d", Locale.US).format(Instant.parse(at).atZone(ZoneId.systemDefault()))
}.getOrDefault(at.take(10))
fun opNames(op: JsonObject): List<String> = (op["names"] as? JsonArray)?.mapNotNull { (it as? JsonPrimitive)?.contentOrNull }.orEmpty()
fun opName(op: JsonObject): String = opNames(op).let { if (it.size > 1) "${it[0]} + ${it.size - 1} more" else it.firstOrNull() ?: "operation ${op.number("id")}" }
/** "344 GB → 37.6 GB" for a freeze, the data size otherwise. */
fun opSizes(op: JsonObject): String {
    val data = op.long("data_bytes")
    val frozen = op.long("frozen_bytes")
    return if (op.text("kind") == "freezing" && data != null && frozen != null) "${storageSize(data)} → ${storageSize(frozen)}"
    else data?.let(::storageSize) ?: ""
}
fun opSaved(op: JsonObject): Long? {
    val data = op.long("data_bytes") ?: return null
    val frozen = op.long("frozen_bytes") ?: return null
    return if (op.text("kind") == "freezing" && op.text("state") == "done") (data - frozen).coerceAtLeast(0) else null
}
fun ratio(data: Long?, frozen: Long?): String? {
    if (data == null || frozen == null || data == 0L || frozen == 0L) return null
    val r = data.toDouble() / frozen
    return (if (r < 10) String.format(Locale.US, "%.1f", r) else String.format(Locale.US, "%.0f", r)) + "×"
}
/** The savings line under the big number. */
fun savingsLine(s: JsonObject?): String {
    val folders = s?.number("folders") ?: 0
    if (s == null || folders == 0L) return "No frozen folders"
    val disks = s.rows("roots").filter { it.number("folders") > 0 }.map { diskName(buildJsonObject { put("path", it.text("cold_store")) }) }.distinct()
    return "$folders frozen ${if (folders == 1L) "folder" else "folders"} · ${storageSize(s.long("data_bytes"))} of data in ${storageSize(s.long("frozen_bytes"))} of archives" +
        if (disks.isEmpty()) "" else " on ${disks.joinToString(", ")}"
}
private fun placementDisk(a: JsonObject): String? = (a["placement"] as? JsonObject)?.let { diskName(buildJsonObject { put("path", it.text("disk_path")) }) }
/** Where a folder's bytes are, in the owner's words. */
fun placementLabel(a: JsonObject): String? {
    val p = a["placement"] as? JsonObject ?: return null
    val disk = placementDisk(a)
    return if (a.text("state") == "frozen") "Archive on $disk" else disk + if (p.text("disk") == "cold") " (cold disk)" else ""
}
/** How long a thaw should take, and the last one measured. */
fun thawTime(a: JsonObject): String? {
    val eta = span(a.decimal("thaw_eta_seconds"))?.let { "about $it" }
    val last = (a["last_thaw"] as? JsonObject)?.let { t -> span(t.decimal("seconds"))?.let { "last $it on ${day(t.text("at"))}" } }
    return listOfNotNull(eta, last).joinToString(" · ").ifEmpty { null }
}
/** A folder with an operation running or waiting on it. */
fun artifactOp(a: JsonObject): String? {
    val op = a.text("op").ifEmpty { return null }
    val operation = a["operation"] as? JsonObject
    val phase = when {
        operation?.text("phase") == "queued" -> "queued"
        operation != null && opStep(operation) == 2 -> "finishing"
        else -> "copying"
    }
    return "${opKind(op).now} · $phase"
}
