package li.rajeshgo.sm.ui.watch

import java.time.Duration
import java.time.OffsetDateTime
import java.time.format.DateTimeParseException
import li.rajeshgo.sm.data.model.ClientSession
import li.rajeshgo.sm.data.model.SessionDetail


data class WatchSection(
    val repoKey: String,
    val repoLabel: String,
    val roots: List<WatchSessionNode>,
)

data class WatchSessionNode(
    val session: ClientSession,
    val sameRepoChildren: List<WatchSessionNode>,
    val crossRepoGroups: List<WatchRepoGroup>,
)

data class WatchRepoGroup(
    val repoKey: String,
    val repoLabel: String,
    val children: List<WatchSessionNode>,
)

fun sessionDisplayName(session: ClientSession): String {
    return session.friendlyName?.takeIf { it.isNotBlank() } ?: session.name.ifBlank { session.id }
}

fun repoKey(workingDir: String): String = workingDir.trim().ifBlank { "unknown" }

fun repoLabel(workingDir: String): String {
    val normalized = repoKey(workingDir)
    return normalized.substringAfterLast('/').ifBlank { normalized } + "/"
}

private fun sortSessions(left: ClientSession, right: ClientSession): Int {
    return compareValuesBy(
        left,
        right,
        { sessionPriority(it) },
        { sessionDisplayName(it).lowercase() },
        { it.id },
    )
}

private fun sessionPriority(session: ClientSession): Int {
    return when {
        isOperationallyActive(session) -> 0
        session.status == "idle" -> 1
        session.status == "stopped" -> 2
        else -> 3
    }
}

fun isActiveSession(session: ClientSession): Boolean = sessionPriority(session) == 0

fun isOperationallyActive(session: ClientSession): Boolean {
    val activity = activityLabel(session.activityState)
    return session.status == "running" || activity == "working" || activity == "thinking" || activity == "waiting"
}

fun projectedStatusLabel(session: ClientSession): String {
    return when {
        session.status == "running" -> "running"
        session.status == "stopped" -> "stopped"
        isOperationallyActive(session) -> "active"
        session.status.isNotBlank() -> session.status
        else -> "idle"
    }
}

private fun nodePriority(node: WatchSessionNode): Int {
    val childPriorities = node.sameRepoChildren.map(::nodePriority)
    val crossRepoPriorities = node.crossRepoGroups.flatMap { group -> group.children.map(::nodePriority) }
    return (listOf(sessionPriority(node.session)) + childPriorities + crossRepoPriorities).minOrNull() ?: 3
}

fun hasActiveBranch(node: WatchSessionNode): Boolean {
    return isActiveSession(node.session) ||
        node.sameRepoChildren.any(::hasActiveBranch) ||
        node.crossRepoGroups.any { group -> group.children.any(::hasActiveBranch) }
}

fun hasIdleBranch(node: WatchSessionNode): Boolean {
    return !isActiveSession(node.session) ||
        node.sameRepoChildren.any(::hasIdleBranch) ||
        node.crossRepoGroups.any { group -> group.children.any(::hasIdleBranch) }
}

private fun sortNodes(nodes: List<WatchSessionNode>): List<WatchSessionNode> {
    return nodes.sortedWith(
        compareBy<WatchSessionNode>({ nodePriority(it) }, { sessionDisplayName(it.session).lowercase() }, { it.session.id })
    )
}

private fun sectionPriority(section: WatchSection): Int {
    return section.roots.map(::nodePriority).minOrNull() ?: 3
}

fun buildSections(sessions: List<ClientSession>): List<WatchSection> {
    val sessionsById = sessions.associateBy { it.id }
    val rootsByRepo = linkedMapOf<String, MutableList<ClientSession>>()
    val sameRepoChildren = linkedMapOf<String, MutableList<ClientSession>>()
    val crossRepoChildren = linkedMapOf<String, LinkedHashMap<String, MutableList<ClientSession>>>()
    val repoKeys = linkedSetOf<String>()

    sessions.forEach { session ->
        val key = repoKey(session.workingDir)
        repoKeys.add(key)
        val parentId = session.parentSessionId?.takeIf { it.isNotBlank() }
        if (parentId == null) {
            rootsByRepo.getOrPut(key) { mutableListOf() }.add(session)
            return@forEach
        }
        val parent = sessionsById[parentId]
        if (parent == null) {
            rootsByRepo.getOrPut(key) { mutableListOf() }.add(session)
            return@forEach
        }
        val parentRepo = repoKey(parent.workingDir)
        if (parentRepo == key) {
            sameRepoChildren.getOrPut(parentId) { mutableListOf() }.add(session)
        } else {
            crossRepoChildren
                .getOrPut(parentId) { linkedMapOf() }
                .getOrPut(key) { mutableListOf() }
                .add(session)
        }
    }

    fun buildNode(session: ClientSession): WatchSessionNode {
        val localChildren = (sameRepoChildren[session.id] ?: emptyList())
            .sortedWith(::sortSessions)
            .map(::buildNode)
        val remoteGroups = (crossRepoChildren[session.id] ?: linkedMapOf()).entries
            .sortedBy { repoLabel(it.key).lowercase() }
            .map { (key, children) ->
                WatchRepoGroup(
                    repoKey = key,
                    repoLabel = repoLabel(key),
                    children = sortNodes(children.sortedWith(::sortSessions).map(::buildNode)),
                )
            }
        return WatchSessionNode(session, sortNodes(localChildren), remoteGroups)
    }

    return repoKeys.sortedBy { repoLabel(it).lowercase() }
        .mapNotNull { key ->
            val roots = sortNodes((rootsByRepo[key] ?: emptyList()).sortedWith(::sortSessions).map(::buildNode))
            if (roots.isEmpty()) null else WatchSection(key, repoLabel(key), roots)
        }
        .sortedWith(compareBy<WatchSection>({ sectionPriority(it) }, { it.repoLabel.lowercase() }, { it.repoKey }))
}

fun filterSections(sections: List<WatchSection>, statusFilter: String, query: String): List<WatchSection> {
    val normalizedQuery = query.trim().lowercase()

    fun matches(session: ClientSession): Boolean {
        if (statusFilter != "all" && !matchesStatusFilter(session, statusFilter)) {
            return false
        }
        if (normalizedQuery.isBlank()) {
            return true
        }
        val haystack = buildString {
            append(session.id)
            append(' ')
            append(session.name)
            append(' ')
            append(sessionDisplayName(session))
            append(' ')
            append(session.tmuxSession)
            append(' ')
            append(session.workingDir)
            append(' ')
            append(session.role ?: "")
            append(' ')
            append(session.provider ?: "")
            append(' ')
            append(session.agentStatusText ?: "")
            append(' ')
            append(session.aliases.joinToString(" "))
        }.lowercase()
        return haystack.contains(normalizedQuery)
    }

    fun filterNode(node: WatchSessionNode): WatchSessionNode? {
        val sameRepoChildren = node.sameRepoChildren.mapNotNull(::filterNode)
        val crossRepoGroups = node.crossRepoGroups
            .map { group -> group.copy(children = group.children.mapNotNull(::filterNode)) }
            .filter { it.children.isNotEmpty() }

        return if (matches(node.session) || sameRepoChildren.isNotEmpty() || crossRepoGroups.isNotEmpty()) {
            node.copy(sameRepoChildren = sameRepoChildren, crossRepoGroups = crossRepoGroups)
        } else {
            null
        }
    }

    return sections.mapNotNull { section ->
        val roots = section.roots.mapNotNull(::filterNode)
        if (roots.isEmpty()) null else section.copy(roots = roots)
    }
}

fun matchesStatusFilter(session: ClientSession, statusFilter: String): Boolean {
    return when (statusFilter) {
        "all" -> true
        "running" -> isOperationallyActive(session)
        "idle" -> !isOperationallyActive(session) && session.status != "stopped"
        "stopped" -> session.status == "stopped"
        else -> session.status == statusFilter
    }
}

fun parseIso(value: String?): OffsetDateTime? {
    if (value.isNullOrBlank()) {
        return null
    }
    return try {
        OffsetDateTime.parse(value)
    } catch (_: DateTimeParseException) {
        null
    }
}

private fun elapsedLabel(seconds: Long): String {
    return when {
        seconds < 60 -> "${seconds}s"
        seconds < 3600 -> "${seconds / 60}m"
        seconds < 86400 -> "${seconds / 3600}h"
        else -> "${seconds / 86400}d"
    }
}

fun ageFromIso(value: String?): String {
    val parsed = parseIso(value) ?: return "-"
    val seconds = Duration.between(parsed, OffsetDateTime.now(parsed.offset)).seconds.coerceAtLeast(0)
    return elapsedLabel(seconds)
}

fun formatAge(lastActivity: String?, activityState: String?): String {
    val parsed = parseIso(lastActivity) ?: return "-"
    val seconds = Duration.between(parsed, OffsetDateTime.now(parsed.offset)).seconds.coerceAtLeast(0)
    return if (activityState == "working" || activityState == "thinking") "${seconds}s" else "${seconds / 60}m"
}

fun formatDateTime(value: String?): String {
    val parsed = parseIso(value) ?: return value ?: "-"
    val local = parsed.toLocalDateTime()
    return "%s %d %02d:%02d".format(local.month.name.lowercase().replaceFirstChar { it.titlecase() }.take(3), local.dayOfMonth, local.hour, local.minute)
}

fun activityLabel(state: String?): String {
    return when (state) {
        "waiting_permission", "waiting_input", "waiting" -> "waiting"
        null, "" -> "idle"
        else -> state
    }
}

fun lastSummary(session: ClientSession): String {
    return when (session.provider) {
        "codex" -> "n/a (no hooks)"
        "codex-app" -> session.lastActionSummary?.let { summary ->
            session.lastActionAt?.let { "$summary (${ageFromIso(it)})" } ?: summary
        } ?: "-"
        else -> session.lastToolName?.let { tool ->
            session.lastToolCall?.let { "$tool (${ageFromIso(it)})" } ?: tool
        } ?: session.lastToolCall?.let { "tool (${ageFromIso(it)})" } ?: "-"
    }
}

fun statusSummary(session: ClientSession): String? {
    val text = session.agentStatusText?.trim()?.takeIf { it.isNotEmpty() } ?: return null
    val ageSuffix = session.agentStatusAt?.let { " (${ageFromIso(it)})" } ?: ""
    return "$text$ageSuffix"
}

fun parentLabel(session: ClientSession, sessionsById: Map<String, ClientSession>): String {
    val parentId = session.parentSessionId?.takeIf { it.isNotBlank() } ?: return "-"
    val parent = sessionsById[parentId] ?: return parentId
    val name = sessionDisplayName(parent)
    return if (name == parentId) parentId else "$name [$parentId]"
}

fun detailLines(session: ClientSession, detail: SessionDetail?): List<String> {
    val lines = mutableListOf(
        "meta: ${sessionDisplayName(session)} [${session.id}] provider=${session.provider ?: "claude"} activity=${activityLabel(session.activityState)} status=${session.status} role=${session.role ?: if (session.isEm) "em" else "-"}${if (session.isMaintainer) " maintainer=yes" else ""}",
        "working dir: ${session.workingDir}",
        "tmux: ${session.tmuxSession}",
        "git remote: ${session.gitRemoteUrl ?: "N/A"}",
        "aliases: ${session.aliases.ifEmpty { listOf("-") }.joinToString(", ")}",
        "current task: ${session.currentTask ?: "No current task"}",
        "context size: ${if (session.contextMonitorEnabled) "${session.tokensUsed} tokens" else "n/a (monitor off)"}",
    )
    session.agentStatusText?.let { lines += "status: \"$it\"${session.agentStatusAt?.let { at -> " (${ageFromIso(at)})" } ?: ""}" }
    session.agentTaskCompletedAt?.let { lines += "task: completed (${ageFromIso(it)})" }
    session.pendingAdoptionProposals.filter { (it.status ?: "pending") == "pending" }.forEach { proposal ->
        val proposerName = proposal.proposerName ?: proposal.proposerSessionId ?: "unknown"
        val proposerId = proposal.proposerSessionId ?: "unknown"
        lines += "adopt: pending from $proposerName [$proposerId]${proposal.createdAt?.let { " (${ageFromIso(it)})" } ?: ""}"
    }
    lines += "last 10 tool calls/actions:"
    lines += (detail?.actionLines ?: listOf("  loading..."))
    lines += "last 10 tail lines:"
    lines += (detail?.tailLines ?: listOf("  loading..."))
    detail?.lastError?.let { lines += "warning: $it" }
    return lines
}
