//! Owner handoff controls for the terminal dashboard.
use super::*;

pub(super) fn context(session: &Value) -> String {
    let policy = &session["handoff"];
    if !policy.is_object() {
        return String::new();
    }
    if !policy["state"].is_null() || policy["has_gauge"] != true {
        return s(policy, "display").into();
    }
    let percent = session["context_percent"]
        .as_f64()
        .map(|p| format!("{p:.0}%"))
        .unwrap_or_else(|| "?%".into());
    if policy["enabled"] == true {
        format!("{percent}/{}", policy["threshold_percent"])
    } else {
        format!("{percent} off")
    }
}

fn command(text: &str) -> Result<Value> {
    Ok(match text.trim() {
        "on" => json!({"enabled": true}),
        "off" => json!({"enabled": false}),
        "default" => json!({"use_default": true}),
        "now" => json!({"ask_now": true}),
        value => {
            let percent = value
                .parse::<u8>()
                .ok()
                .filter(|p| (1..=100).contains(p))
                .ok_or_else(|| anyhow!("Use on, off, default, an integer 1–100, or now"))?;
            json!({"threshold_percent": percent})
        }
    })
}

pub(super) fn apply(worker: &Worker, view: &mut View, id: &str, text: &str) -> Result<()> {
    if view.busy {
        bail!("An operation is already in progress");
    }
    let body = command(text)?;
    view.handoff = None;
    if body["ask_now"] == true {
        view.handoff = Some((id.into(), Instant::now()));
        view.flash = format!("Press H within 5s to ask {id} to hand off now");
        return Ok(());
    }
    send_action(
        worker,
        view,
        "PUT",
        format!("/sessions/{}/handoff-policy", enc(id)),
        body,
        false,
    )
}

pub(super) fn confirm_now(worker: &Worker, view: &mut View, id: &str) -> Result<bool> {
    let confirmed = view
        .handoff
        .take()
        .is_some_and(|(armed, at)| armed == id && at.elapsed() < Duration::from_secs(5));
    if confirmed {
        send_action(
            worker,
            view,
            "PUT",
            format!("/sessions/{}/handoff-policy", enc(id)),
            json!({"ask_now": true}),
            false,
        )?;
    }
    Ok(confirmed)
}

fn fields(defaults: &Value) -> Vec<(String, String, Value)> {
    let mut providers = BTreeSet::from(["claude", "codex-fork", "codex-app"]);
    if let Some(values) = defaults["providers"].as_object() {
        providers.extend(values.keys().map(String::as_str));
    }
    let mut rows: Vec<_> = providers
        .into_iter()
        .map(|p| {
            (
                format!("providers.{p}"),
                format!("Enable {p}"),
                defaults["providers"][p].as_bool().unwrap_or(false).into(),
            )
        })
        .collect();
    for (key, label) in [
        ("threshold_percent", "Context threshold (%)"),
        ("ask_on_codex_review", "Ask on Codex review request"),
        ("ask_on_doc_review", "Ask on doc review request"),
        ("review_floor_percent", "Review floor (%)"),
        ("reminder_percent", "Reminder at (%)"),
    ] {
        rows.push((key.into(), label.into(), defaults[key].clone()));
    }
    rows
}

fn default_body(field: &str, text: &str) -> Result<Value> {
    let boolean = field.starts_with("providers.") || field.starts_with("ask_on_");
    let value = if boolean {
        match text.trim() {
            "on" | "true" => json!(true),
            "off" | "false" => json!(false),
            _ => bail!("Use on or off"),
        }
    } else {
        let number: f64 = text
            .trim()
            .parse()
            .map_err(|_| anyhow!("Enter a percentage"))?;
        let minimum = if field == "review_floor_percent" {
            0.0
        } else {
            f64::MIN_POSITIVE
        };
        if !number.is_finite() || number < minimum || number > 100.0 {
            bail!(
                "Percentage must be {}–100",
                if minimum == 0.0 {
                    "0"
                } else {
                    "greater than 0"
                }
            );
        }
        json!(number)
    };
    Ok(if let Some(provider) = field.strip_prefix("providers.") {
        json!({"providers": {provider: value}})
    } else {
        json!({field: value})
    })
}

fn save_default(worker: &Worker, view: &mut View, field: &str, text: &str) -> Result<()> {
    send_action(
        worker,
        view,
        "PUT",
        "/handoff-defaults".into(),
        default_body(field, text)?,
        false,
    )
}

pub(super) fn draw_defaults(view: &View) -> Result<()> {
    let (h, w) = size();
    print!("\x1b[H\x1b[2JHandoff defaults — arrows/j/k select, Enter edit, q/Esc back\r\n");
    let rows = fields(&view.defaults);
    let start = view.default_index.saturating_sub(h.saturating_sub(4));
    for (i, (_, label, value)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(h.saturating_sub(3))
    {
        let value = if view.defaults.is_null() {
            "loading…".into()
        } else {
            value.to_string()
        };
        println!(
            "{}\r",
            clipped(
                &format!(
                    "{} {label}: {value}",
                    if i == view.default_index { ">" } else { " " }
                ),
                w.saturating_sub(1)
            )
        );
    }
    print!("\x1b[{h};1H{}", clipped(&view.flash, w.saturating_sub(1)));
    io::stdout().flush()?;
    Ok(())
}

pub(super) fn defaults_key(key: Key, worker: &Worker, view: &mut View) -> Result<()> {
    let rows = fields(&view.defaults);
    match key {
        Key::Esc | Key::Char('q') => view.defaults_open = false,
        Key::Down | Key::Char('j') => {
            view.default_index = (view.default_index + 1).min(rows.len() - 1)
        }
        Key::Up | Key::Char('k') => view.default_index = view.default_index.saturating_sub(1),
        Key::Enter if !view.busy && !view.defaults.is_null() => {
            let (field, label, value) = &rows[view.default_index];
            let choices = if value.is_boolean() {
                "on/off"
            } else {
                "percent"
            };
            if let Some(text) = prompt(&format!("{label} [{choices}]> "))? {
                save_default(worker, view, field, &text)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_validate_and_merge_one_field() {
        assert_eq!(
            default_body("providers.codex-fork", "on").unwrap(),
            json!({"providers":{"codex-fork":true}})
        );
        assert_eq!(
            default_body("review_floor_percent", "0").unwrap(),
            json!({"review_floor_percent":0.0})
        );
        for invalid in ["0", "101", "NaN", "inf", "-1"] {
            assert!(default_body("threshold_percent", invalid).is_err());
        }
        assert!(default_body("ask_on_doc_review", "yes").is_err());
    }
}
