//! `sm autopm events` (sm#2173; Auto PM spec, Appendix A.7): the owner
//! activity log, following cursors to the end.
use super::*;

const PAGE: usize = 10_000;

#[derive(Args)]
pub(super) struct AutopmArgs {
    #[command(subcommand)]
    command: AutopmCommand,
}

#[derive(Subcommand)]
enum AutopmCommand {
    /// The owner activity log: one row per owner action
    Events {
        /// How far back, for example 7d or 12h
        #[arg(long, default_value = "7d")]
        since: String,
        /// android, web, agent_prompt or terminal
        #[arg(long)]
        surface: Option<String>,
        /// An action name, for example doc.open
        #[arg(long)]
        action: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub(super) fn run(client: &ApiClient, args: AutopmArgs) -> Result<()> {
    let AutopmCommand::Events {
        since,
        surface,
        action,
        json,
    } = args.command;
    let since_ms =
        (OffsetDateTime::now_utc().unix_timestamp() - parse_duration_seconds(&since)?) * 1000;
    let events = fetch_all(client, since_ms, surface.as_deref(), action.as_deref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&Value::Array(events))?);
        return Ok(());
    }
    for event in &events {
        println!("{}", event_line(event));
    }
    if events.is_empty() {
        println!("no owner events since {since} ago");
    }
    Ok(())
}

fn fetch_all(
    client: &ApiClient,
    since_ms: i64,
    surface: Option<&str>,
    action: Option<&str>,
) -> Result<Vec<Value>> {
    let mut base = format!("/autopm/events?since_ms={since_ms}&limit={PAGE}");
    for (name, value) in [("surface", surface), ("action", action)] {
        if let Some(value) = value {
            base.push_str(&format!("&{name}={}", encode_query_component(value)));
        }
    }
    let mut events = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match cursor.as_deref() {
            Some(cursor) => format!("{base}&cursor={}", encode_query_component(cursor)),
            None => base.clone(),
        };
        let page = client.get_json(&path)?;
        events.extend(page["events"].as_array().cloned().unwrap_or_default());
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => return Ok(events),
        }
    }
}

fn event_line(event: &Value) -> String {
    let at = event["at_ms"]
        .as_i64()
        .and_then(|ms| OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok())
        .map(|at| {
            at.to_offset(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC))
        })
        .and_then(|at| at.format(&Rfc3339).ok())
        .unwrap_or_default();
    let text = |key: &str| event[key].as_str().unwrap_or("").to_owned();
    let target = match (event["target_kind"].as_str(), event["target_id"].as_str()) {
        (Some(kind), Some(id)) => format!("{kind}:{id}"),
        (None, Some(id)) => id.to_owned(),
        _ => "-".to_owned(),
    };
    let nav = match (event["nav"].as_str(), event["notice_id"].as_str()) {
        (Some(nav), Some(notice)) => format!("{nav} {notice}"),
        (Some(nav), None) => nav.to_owned(),
        _ => String::new(),
    };
    let status = event["status"]
        .as_i64()
        .map(|status| status.to_string())
        .unwrap_or_default();
    format!(
        "{at}  {:<12}  {:<22}  {target}  {}  {nav}  {status}",
        text("surface"),
        text("action"),
        text("screen"),
    )
    .trim_end()
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_line_shows_target_screen_nav_and_status() {
        let line = event_line(&json!({
            "at_ms": 1_792_040_000_000i64, "surface": "android", "action": "inbox.thread",
            "target_kind": "thread", "target_id": "t_91", "screen": "inbox/thread",
            "nav": "push", "notice_id": "n_442", "status": 200
        }));
        assert!(line.contains("android"), "{line}");
        assert!(line.contains("inbox.thread  "), "{line}");
        assert!(
            line.contains("thread:t_91  inbox/thread  push n_442  200"),
            "{line}"
        );
        let bare =
            event_line(&json!({"at_ms": 0, "surface": "terminal", "action": "terminal.input"}));
        assert!(bare.ends_with("terminal.input          -"), "{bare}");
    }
}
