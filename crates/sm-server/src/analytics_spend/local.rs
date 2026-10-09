//! Local usage has calendar weeks (Monday, UTC), tokens and elapsed model
//! request time. It never enters hosted quota pricing or meter reconciliation.
use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct LocalSpend {
    pub models: Vec<ModelRow>,
    pub days: Vec<LocalDay>,
    pub busy_hours: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalDay {
    pub date: String,
    pub busy_hours: f64,
}

pub(super) fn report(
    sources: &SpendSources<'_>,
    range: SpendRange,
    now: OffsetDateTime,
) -> Result<SpendReport> {
    let day_ns = 86_400 * NANOS_PER_SECOND;
    let midnight = now.date().midnight().assume_utc().unix_timestamp_nanos();
    let monday = midnight - i128::from(now.weekday().number_days_from_monday()) * day_ns;
    let (start, end) = match range {
        SpendRange::Week => (monday, now.unix_timestamp_nanos()),
        SpendRange::LastWeek => (monday - 7 * day_ns, monday),
        SpendRange::FourWeeks => (
            now.unix_timestamp_nanos() - 28 * day_ns,
            now.unix_timestamp_nanos(),
        ),
    };
    let mut report = empty_report(
        format_nanos(now.unix_timestamp_nanos()),
        "local",
        range,
        format_nanos(start),
        format_nanos(end),
    );
    report.root.label = "Local".into();
    report.notes = vec!["Days and weeks use UTC; weeks start Monday. Model-busy hours count overlapping recorded requests once. Older requests without timing metadata are excluded from hours.".into()];
    let mut models: BTreeMap<String, (i64, Tokens)> = BTreeMap::new();
    let mut intervals = Vec::new();
    if sources.usage_db.exists() {
        let usage = open_read_only(sources.usage_db)?;
        let accounts = provider_accounts(&usage, "local")?;
        for turn in load_turns(&usage, "local", &accounts, start, end)? {
            let entry = models.entry(turn.model).or_default();
            entry.0 += turn.turns;
            entry.1.add(&turn.tokens);
        }
        if table_exists(&usage, "local_request_intervals")? {
            let mut statement = usage.prepare(
                "SELECT start_ms, end_ms FROM local_request_intervals
                 WHERE end_ms > ?1 AND start_ms < ?2 ORDER BY start_ms, end_ms",
            )?;
            for row in statement.query_map(
                params![(start / 1_000_000) as i64, (end / 1_000_000) as i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )? {
                let (a, b) = row?;
                intervals.push((i128::from(a) * 1_000_000, i128::from(b) * 1_000_000));
            }
        }
    }
    let models: Vec<_> = models
        .into_iter()
        .map(|(model, (turns, tokens))| {
            report.total.tokens += tokens.input
                + tokens.output
                + tokens.cache_read
                + tokens.cache_write_5m
                + tokens.cache_write_1h;
            ModelRow {
                model,
                effort: None,
                turns,
                percent: 0.0,
                tokens: TokenBreakdown::from(&tokens),
            }
        })
        .collect();
    report.root.tokens = report.total.tokens;
    let days = daily_union(&intervals, start, end);
    report.local = Some(LocalSpend {
        busy_hours: days.iter().map(|day| day.busy_hours).sum(),
        models,
        days,
    });
    Ok(report)
}

fn daily_union(intervals: &[(i128, i128)], start: i128, end: i128) -> Vec<LocalDay> {
    let day_ns = 86_400 * NANOS_PER_SECOND;
    let mut intervals = intervals.to_vec();
    intervals.sort_unstable();
    let mut days = Vec::new();
    let mut day = start.div_euclid(day_ns) * day_ns;
    while day < end {
        let lo = day.max(start);
        let hi = (day + day_ns).min(end);
        let mut through = lo;
        let mut busy = 0;
        for &(a, b) in &intervals {
            let a = a.max(lo).max(through);
            let b = b.min(hi);
            if b > a {
                busy += b - a;
                through = b;
            }
        }
        days.push(LocalDay {
            date: format_nanos(day)[..10].into(),
            busy_hours: busy as f64 / (3_600 * NANOS_PER_SECOND) as f64,
        });
        day += day_ns;
    }
    days
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn union_clips_range_splits_midnight_and_does_not_double_count_seats() {
        let hour = 3_600 * NANOS_PER_SECOND;
        let days = daily_union(
            &[
                (22 * hour, 26 * hour),
                (23 * hour, 25 * hour),
                (25 * hour, 28 * hour),
            ],
            23 * hour,
            27 * hour,
        );
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].busy_hours, 1.0);
        assert_eq!(days[1].busy_hours, 3.0);
    }
}
