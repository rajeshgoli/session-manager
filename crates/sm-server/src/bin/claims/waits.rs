//! `sm await` (#2477): record that this session waits on another seat, a
//! ticket, a queue job or work outside sm for a deliverable, until an ETA.
//! The server ends the wait when the awaited seat messages you, the ticket
//! closes, the seat retires, the job ends, or the ETA passes.

use super::*;

#[derive(Args)]
pub(crate) struct AwaitArgs {
    /// What you wait on: a seat (id or name), a ticket (#N or
    /// owner/repo#N), a queue job (queue:<job id>), or work outside sm as
    /// <kind>:<id> (cron:3fa9, modal:ap-12, gh-run:4567). Without one, list
    /// your open waits and the waits on you
    target: Option<String>,
    /// What you wait for (required to open a wait)
    #[arg(long = "for", value_name = "DELIVERABLE")]
    deliverable: Option<String>,
    /// ETA: a duration (45m, 6h, 2d) or an RFC 3339 time, at most 72h away
    #[arg(long, value_name = "ETA")]
    until: Option<String>,
    /// End your wait on TARGET, or all your waits
    #[arg(long, conflicts_with_all = ["deliverable", "until"])]
    cancel: bool,
}

pub(crate) fn run_await(client: &ApiClient, args: AwaitArgs) -> Result<()> {
    let session_id = managed_session_id("sm await")?;
    let target = args
        .target
        .as_deref()
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(qualify_target)
        .transpose()?;
    if args.cancel {
        let response = client.request(
            "POST",
            "/claims/waits/cancel",
            Some(json!({"waiter_session_id": session_id, "target": target})),
        )?;
        let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        return finish(cancel_output(target.as_deref(), response.status, &body));
    }
    let Some(target) = target else {
        if args.deliverable.is_some() || args.until.is_some() {
            bail!(
                "sm await needs a target: a seat, #N, owner/repo#N, queue:<job id> or <kind>:<id>"
            );
        }
        let payload = client.get_json(&format!(
            "/claims/waits?session_id={}",
            encode_query_component(&session_id)
        ))?;
        for line in wait_list_lines(&payload, OffsetDateTime::now_utc()) {
            println!("{line}");
        }
        return Ok(());
    };
    let (Some(deliverable), Some(until)) = (args.deliverable, args.until) else {
        bail!("sm await <target> needs --for \"<deliverable>\" and --until <ETA>");
    };
    let until = until_rfc3339(&until, OffsetDateTime::now_utc())?;
    let response = client.request(
        "POST",
        "/claims/waits",
        Some(json!({
            "waiter_session_id": session_id, "target": target,
            "for": deliverable, "until": until,
        })),
    )?;
    let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    finish(open_output(response.status, &body))
}

/// `#N` takes the repo of the cwd checkout, as `sm ticket N` does.
fn qualify_target(target: &str) -> Result<String> {
    match target.strip_prefix('#') {
        Some(number) => {
            let cwd = env::current_dir()?;
            let repo = resolve_claim_target(&ProcessTools, &cwd, None)?.repo;
            Ok(format!("{repo}#{number}"))
        }
        None => Ok(target.to_owned()),
    }
}

/// A duration from `now`, or an RFC 3339 time as given.
pub(crate) fn until_rfc3339(until: &str, now: OffsetDateTime) -> Result<String> {
    let until = until.trim();
    if let Ok(seconds) = parse_duration_seconds(until) {
        return Ok((now + time::Duration::seconds(seconds)).format(&Rfc3339)?);
    }
    if OffsetDateTime::parse(until, &Rfc3339).is_ok() {
        return Ok(until.to_owned());
    }
    bail!("--until must be a duration (45m, 6h, 2d) or an RFC 3339 time, not {until}")
}

/// `far-2171 (4c1d2e3f)` or `#2171 "Title"`.
fn target_text(wait: &Value) -> String {
    if wait["target_kind"] == "ticket" {
        let number = wait["number"].as_i64().unwrap_or_default();
        return match wait["title"].as_str().filter(|title| !title.is_empty()) {
            Some(title) => format!("#{number} \"{title}\""),
            None => format!("#{number}"),
        };
    }
    if let Some(reference) = wait["target_ref"].as_str() {
        return reference.to_owned();
    }
    seat_text(&wait["target_name"], &wait["target_session_id"])
}

fn seat_text(name: &Value, id: &Value) -> String {
    let id: String = id.as_str().unwrap_or_default().chars().take(8).collect();
    match name.as_str().filter(|name| !name.is_empty() && *name != id) {
        Some(name) => format!("{name} ({id})"),
        None => id,
    }
}

pub(crate) fn open_output(status: u16, body: &Value) -> Printed {
    if !(200..300).contains(&status) {
        return Printed {
            stdout: Vec::new(),
            stderr: vec![api_detail(status, body)],
            exit: 1,
        };
    }
    let verb = if body["created"] == true {
        "Waiting on"
    } else {
        "Updated wait on"
    };
    Printed {
        stdout: vec![format!(
            "{verb} {} for \"{}\" until {}.",
            target_text(body),
            body["for"].as_str().unwrap_or_default(),
            body["until_text"].as_str().unwrap_or_default()
        )],
        stderr: Vec::new(),
        exit: 0,
    }
}

pub(crate) fn cancel_output(target: Option<&str>, status: u16, body: &Value) -> Printed {
    if !(200..300).contains(&status) {
        return Printed {
            stdout: Vec::new(),
            stderr: vec![api_detail(status, body)],
            exit: 1,
        };
    }
    let count = body["cancelled"].as_i64().unwrap_or_default();
    let line = match target {
        Some(target) => format!("Cancelled wait on {target}."),
        None if count == 1 => "Cancelled 1 wait.".to_owned(),
        None => format!("Cancelled {count} waits."),
    };
    Printed {
        stdout: vec![line],
        stderr: Vec::new(),
        exit: 0,
    }
}

/// `Waiting on:` and `Awaited by:` sections, one line per open wait.
pub(crate) fn wait_list_lines(payload: &Value, now: OffsetDateTime) -> Vec<String> {
    let line = |wait: &Value, who: String| {
        let age = wait["since"]
            .as_str()
            .and_then(|since| OffsetDateTime::parse(since, &Rfc3339).ok())
            .map(|since| short_age(now - since))
            .unwrap_or_default();
        format!(
            "  {who} · \"{}\" · until {} · {age}",
            wait["for"].as_str().unwrap_or_default(),
            wait["until_text"].as_str().unwrap_or_default()
        )
    };
    let mine = payload["waiting_on"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let on_me = payload["awaited_by"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if mine.is_empty() && on_me.is_empty() {
        return vec!["No open waits.".to_owned()];
    }
    let mut lines = Vec::new();
    if !mine.is_empty() {
        lines.push("Waiting on:".to_owned());
        lines.extend(mine.iter().map(|wait| line(wait, target_text(wait))));
    }
    if !on_me.is_empty() {
        lines.push("Awaited by:".to_owned());
        lines.extend(on_me.iter().map(|wait| {
            line(
                wait,
                seat_text(&wait["waiter_name"], &wait["waiter_session_id"]),
            )
        }));
    }
    lines
}

/// `12m`, `3h`, `2d`.
fn short_age(age: time::Duration) -> String {
    let minutes = age.whole_minutes().max(0);
    match minutes {
        0..=59 => format!("{minutes}m"),
        60..=2879 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::parse("2026-10-21T22:10:00Z", &Rfc3339).unwrap()
    }

    #[test]
    fn until_takes_a_duration_or_an_rfc3339_time() {
        assert_eq!(until_rfc3339("7h", now()).unwrap(), "2026-10-22T05:10:00Z");
        assert_eq!(
            until_rfc3339("2026-10-22T05:30:00Z", now()).unwrap(),
            "2026-10-22T05:30:00Z"
        );
        assert!(until_rfc3339("Thursday", now()).is_err());
    }

    #[test]
    fn open_and_list_lines() {
        let wait = json!({"created": true, "target_kind": "seat", "target_name": "far-2171",
            "target_session_id": "4c1d2e3f", "for": "hashes", "until_text": "Thu 22 Oct 05:30 UTC",
            "since": "2026-10-21T19:10:00Z", "waiter_name": "far-2139", "waiter_session_id": "7e317fcd"});
        assert_eq!(
            open_output(201, &wait).stdout,
            ["Waiting on far-2171 (4c1d2e3f) for \"hashes\" until Thu 22 Oct 05:30 UTC."]
        );
        let mut updated = wait.clone();
        updated["created"] = json!(false);
        updated["target_kind"] = json!("ticket");
        updated["number"] = json!(2171);
        updated["title"] = json!("Train final weights");
        assert_eq!(
            open_output(200, &updated).stdout,
            ["Updated wait on #2171 \"Train final weights\" for \"hashes\" until Thu 22 Oct 05:30 UTC."]
        );
        let refused = open_output(400, &json!({"detail": "you cannot wait on yourself"}));
        assert_eq!(
            (refused.stderr, refused.exit),
            (vec!["you cannot wait on yourself".to_owned()], 1)
        );
        assert_eq!(
            wait_list_lines(
                &json!({"waiting_on": [wait.clone()], "awaited_by": [wait]}),
                now()
            ),
            [
                "Waiting on:",
                "  far-2171 (4c1d2e3f) · \"hashes\" · until Thu 22 Oct 05:30 UTC · 3h",
                "Awaited by:",
                "  far-2139 (7e317fcd) · \"hashes\" · until Thu 22 Oct 05:30 UTC · 3h",
            ]
        );
        assert_eq!(
            wait_list_lines(&json!({"waiting_on": [], "awaited_by": []}), now()),
            ["No open waits."]
        );
        assert_eq!(
            cancel_output(None, 200, &json!({"cancelled": 2})).stdout,
            ["Cancelled 2 waits."]
        );
    }
}
