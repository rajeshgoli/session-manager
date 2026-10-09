package li.rajeshgo.sm.ui.links

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import java.time.OffsetDateTime
import li.rajeshgo.sm.data.model.BoardHolder
import li.rajeshgo.sm.data.model.BoardJob
import li.rajeshgo.sm.data.model.BoardPr
import li.rajeshgo.sm.data.model.BoardThread
import li.rajeshgo.sm.ui.queue.secondsBetween
import li.rajeshgo.sm.ui.queue.shortDuration
import li.rajeshgo.sm.ui.theme.Amber
import li.rajeshgo.sm.ui.theme.Border
import li.rajeshgo.sm.ui.theme.Cyan
import li.rajeshgo.sm.ui.theme.Emerald
import li.rajeshgo.sm.ui.theme.Fuchsia
import li.rajeshgo.sm.ui.theme.Rose
import li.rajeshgo.sm.ui.theme.TextSecondary
import li.rajeshgo.sm.ui.theme.Violet

/** Job chips shown before the rest fold into "+{n} jobs". */
const val LINK_JOB_CHIPS = 3

/**
 * One chip of the Links line (spec 1782 H4); [onTerminal] adds the agent chip's ⌨.
 * [onClick] is last, so a trailing lambda is always the chip's click.
 */
data class LinkChip(
    val text: String,
    val color: Color,
    val onTerminal: (() -> Unit)? = null,
    val onClick: (() -> Unit)? = null,
)

private fun age(from: String?, now: OffsetDateTime): String? =
    secondsBetween(from, now)?.let { shortDuration(it) }

/** "PR #1785 · open · your review, round 3 · waiting 4m". */
fun prChipText(pr: BoardPr, now: OffsetDateTime): String {
    val parts = mutableListOf("PR #${pr.number}", pr.state.lowercase())
    pr.review?.let { review ->
        parts += "${if (review.by == "you") "your review" else "Codex review"}, round ${review.round}"
        review.verdict?.let { parts += it.replace('_', ' ') }
        if (review.waitingSince != null) age(review.waitingSince, now)?.let { parts += "waiting $it" }
    }
    return parts.joinToString(" · ")
}

/** Magenta while the PR waits on you, amber while it waits on Codex. */
fun prChipColor(pr: BoardPr): Color {
    val review = pr.review?.takeIf { it.waitingSince != null } ?: return Violet
    return if (review.by == "you") Fuchsia else Amber
}

/** "1858-run · running 1h 19m"; quiet counts from when the job went quiet. */
fun jobChipText(job: BoardJob, now: OffsetDateTime): String {
    val since = if (job.state == "quiet") job.quietSince ?: job.since else job.since
    return listOfNotNull(job.label, age(since, now)?.let { "${job.state} $it" } ?: job.state).joinToString(" · ")
}

fun jobChipColor(job: BoardJob): Color = when (job.state) {
    "running" -> Emerald
    "waiting" -> Amber
    else -> Rose
}

/** Up to [LINK_JOB_CHIPS] job chips, then "+{n} jobs". */
fun jobChips(jobs: List<BoardJob>, now: OffsetDateTime, onClick: (BoardJob) -> Unit): List<LinkChip> {
    val shown = jobs.take(LINK_JOB_CHIPS).map { job -> LinkChip(jobChipText(job, now), jobChipColor(job)) { onClick(job) } }
    val more = jobs.size - shown.size
    return if (more > 0) shown + LinkChip("+$more jobs", TextSecondary) { onClick(jobs[LINK_JOB_CHIPS]) } else shown
}

fun threadChipText(thread: BoardThread): String =
    if (thread.needsYou) "Inbox · question" else "Inbox · ${thread.count}"

fun providerLabel(provider: String): String = when (provider) {
    "opencode" -> "Local"
    "claude" -> "Claude"
    "" -> ""
    else -> "Codex"
}

/** "sm-1776 · Codex · ● Working 1m" (appendix B's agent fact). */
fun agentChipText(holder: BoardHolder, now: OffsetDateTime): String {
    val fact = (if (holder.state == "working") "● Working" else "○ Idle") +
        (age(holder.since, now)?.let { " $it" } ?: "")
    return listOf(holder.name, providerLabel(holder.provider), fact).filter { it.isNotBlank() }.joinToString(" · ")
}

/** The Links line: ticket, PRs, agent, jobs, thread, docs, each a chip that opens its item. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun LinksRow(chips: List<LinkChip>, modifier: Modifier = Modifier) {
    if (chips.isEmpty()) return
    FlowRow(
        modifier = modifier,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        chips.forEach { chip -> Chip(chip) }
    }
}

@Composable
private fun Chip(chip: LinkChip) {
    val shape = RoundedCornerShape(999.dp)
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .clip(shape)
            .border(BorderStroke(1.dp, if (chip.color == TextSecondary) Border else chip.color.copy(alpha = 0.35f)), shape),
    ) {
        Text(
            chip.text,
            color = chip.color,
            style = MaterialTheme.typography.labelSmall,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = (chip.onClick?.let { Modifier.clickable(onClick = it) } ?: Modifier)
                .padding(horizontal = 8.dp, vertical = 3.dp),
        )
        chip.onTerminal?.let { onTerminal ->
            Text(
                "⌨",
                color = Cyan,
                style = MaterialTheme.typography.labelMedium,
                modifier = Modifier.clickable(onClick = onTerminal).padding(start = 2.dp, end = 8.dp, top = 2.dp, bottom = 2.dp),
            )
        }
    }
}
