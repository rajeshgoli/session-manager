//! `sm autopm` (Auto PM spec `docs/working/2170_auto_pm.html`): `events`,
//! the owner activity log, following cursors to the end (sm#2173, A.7);
//! `switch`, `exp` and `budget` (sm#2174, B and C).
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
    /// Live switches: list them, or turn one on or off
    Switch {
        #[command(subcommand)]
        command: SwitchCommand,
    },
    /// Experiments: record, change, list and show them
    Exp {
        #[command(subcommand)]
        command: ExpCommand,
    },
    /// This week's budget per provider: allowance, spent, reserved, remaining
    Budget {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SwitchCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    On {
        key: String,
        #[arg(long)]
        reason: Option<String>,
    },
    Off {
        key: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
enum ExpCommand {
    /// Record an experiment from a JSON file (`-` for stdin)
    New { file: String },
    /// Change an experiment: a state move, a note, or fields from a JSON file
    Set {
        id: String,
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        file: Option<String>,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

fn read_json(file: &str) -> Result<Value> {
    let text = if file == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        text
    } else {
        std::fs::read_to_string(file).with_context(|| format!("cannot read {file}"))?
    };
    serde_json::from_str(&text).with_context(|| format!("{file} is not JSON"))
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub(super) fn run(client: &ApiClient, args: AutopmArgs) -> Result<()> {
    match args.command {
        AutopmCommand::Events {
            since,
            surface,
            action,
            json,
        } => events(client, &since, surface, action, json),
        AutopmCommand::Switch { command } => switch(client, command),
        AutopmCommand::Exp { command } => exp(client, command),
        AutopmCommand::Budget { json } => {
            let budget = client.get_json("/autopm/budget")?;
            if json {
                return print_json(&budget);
            }
            println!("{}", budget_text(&budget));
            Ok(())
        }
    }
}

fn switch(client: &ApiClient, command: SwitchCommand) -> Result<()> {
    let (key, on, reason) = match command {
        SwitchCommand::List { json } => {
            let list = client.get_json("/autopm/switches")?;
            if json {
                return print_json(&list);
            }
            if list["enabled"] != true {
                println!("Auto PM is off: every switch reads as off");
            }
            for row in list["switches"].as_array().into_iter().flatten() {
                println!(
                    "{:<36} {:<3} {:<5} {:<10} {}",
                    row["key"].as_str().unwrap_or(""),
                    if row["on"] == true { "on" } else { "off" },
                    if row["held"] == true { "held" } else { "" },
                    row["changed_by"].as_str().unwrap_or(""),
                    row["experiment_id"].as_str().unwrap_or(""),
                );
            }
            return Ok(());
        }
        SwitchCommand::On { key, reason } => (key, true, reason),
        SwitchCommand::Off { key, reason } => (key, false, reason),
    };
    let path = format!("/autopm/switches/{}", encode_query_component(&key));
    let result = client.put_json(&path, json!({"on": on, "reason": reason}))?;
    let row = &result["switch"];
    println!(
        "{key} {}{}",
        if row["on"] == true { "on" } else { "off" },
        if row["held"] == true { " (held)" } else { "" }
    );
    Ok(())
}

fn exp(client: &ApiClient, command: ExpCommand) -> Result<()> {
    match command {
        ExpCommand::New { file } => {
            let created = client.post_json("/autopm/experiments", read_json(&file)?)?;
            println!("{}", experiment_line(&created["experiment"]));
        }
        ExpCommand::Set {
            id,
            state,
            note,
            file,
        } => {
            let mut body = match file {
                Some(file) => read_json(&file)?,
                None => json!({}),
            };
            if let Some(state) = state {
                body["state"] = json!(state);
            }
            if let Some(note) = note {
                body["decision_note"] = json!(note);
            }
            let path = format!("/autopm/experiments/{}", encode_query_component(&id));
            let changed = client.patch_json(&path, body)?;
            println!("{}", experiment_line(&changed["experiment"]));
        }
        ExpCommand::List { json } => {
            let list = client.get_json("/autopm/experiments")?;
            if json {
                return print_json(&list);
            }
            for x in list["experiments"].as_array().into_iter().flatten() {
                println!("{}", experiment_line(x));
            }
        }
        ExpCommand::Show { id, json } => {
            let path = format!("/autopm/experiments/{}", encode_query_component(&id));
            let shown = client.get_json(&path)?;
            if json {
                return print_json(&shown);
            }
            let x = &shown["experiment"];
            println!("{}", experiment_line(x));
            println!("{}", x["hypothesis"].as_str().unwrap_or(""));
            for key in [
                "switch_key",
                "plan_pr",
                "plan_commit",
                "live_at_ms",
                "measure_at_ms",
            ] {
                if !x[key].is_null() {
                    println!("{key}: {}", x[key]);
                }
            }
            if let Some(note) = x["decision_note"].as_str() {
                println!("note: {note}");
            }
            for m in shown["measurements"].as_array().into_iter().flatten() {
                println!(
                    "attempt {}: {} {}",
                    m["attempt"],
                    m["status"].as_str().unwrap_or(""),
                    m["decision"].as_str().or(m["error"].as_str()).unwrap_or("")
                );
            }
        }
    }
    Ok(())
}

fn experiment_line(x: &Value) -> String {
    format!(
        "{:<36} {:<14} {:<8} {}",
        x["id"].as_str().unwrap_or(""),
        x["state"].as_str().unwrap_or(""),
        x["path"].as_str().unwrap_or(""),
        x["title"].as_str().unwrap_or("")
    )
}

fn budget_text(budget: &Value) -> String {
    let mut lines = Vec::new();
    if budget["enabled"] != true {
        lines.push("Auto PM is off; no budget is in force.".to_owned());
    }
    for provider in ["claude", "codex"] {
        let p = &budget["providers"][provider];
        if p.is_null() {
            lines.push(format!("{provider}: no current weekly window"));
            continue;
        }
        let n = |v: &Value| v.as_f64().unwrap_or(0.0);
        lines.push(format!(
            "{provider}: allowance {:.1}, spent {:.1}, reserved {:.1}, remaining {:.1}{} (resets {})",
            n(&p["allowance"]),
            n(&p["spent"]),
            n(&p["reserved"]),
            n(&p["remaining"]),
            if p["skipped"] == true { ", skipped" } else { "" },
            p["resets_at"].as_str().unwrap_or("?"),
        ));
        for part in ["pm", "reviews"] {
            lines.push(format!(
                "  {part}: held {:.1}, spent {:.1}, left {:.1}",
                n(&p[part]["held"]),
                n(&p[part]["spent"]),
                n(&p[part]["left"])
            ));
        }
    }
    lines.join("\n")
}

fn events(
    client: &ApiClient,
    since: &str,
    surface: Option<String>,
    action: Option<String>,
    json: bool,
) -> Result<()> {
    let since_ms =
        (OffsetDateTime::now_utc().unix_timestamp() - parse_duration_seconds(since)?) * 1000;
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
