//! Owner settings (sm#1718; spec `1710_web_redesign.html`, appendices D4 and
//! D5): what New agent and board Start fill in, and the queue slot and
//! terminal attach limits that apply without a restart (sm#1763). The phone
//! and the web share them.
//!
//! Everything here is pure. `sessions` owns persistence: the store keeps, per
//! settings key, only the fields the owner set (`{value, updated_at}`), and
//! the effective object is the defaults overlaid with them. A `null` in a
//! `PUT` removes the owner's value, so the default applies again.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::config::AppConfig;
use crate::queue::QueueAdmissionPolicy;
use crate::sessions::expand_home;

/// Top-level key of the stored settings in the session store.
pub const STORE_KEY: &str = "owner_settings";
/// The settings keys, one stored row each.
const KEYS: [&str; 4] = ["new_agent", "queue_limits", "terminal_limits", "reviews"];
/// Objects stored whole: a `PUT` replaces them rather than merging into them.
const WHOLE_VALUES: [&str; 1] = ["repo_short"];
const PLACEHOLDERS: [&str; 7] = [
    "ticket",
    "number",
    "repo",
    "repo_name",
    "repo_short",
    "title",
    "url",
];
const CLAUDE_EFFORTS: [&str; 4] = ["low", "medium", "high", "max"];
const CODEX_EFFORTS: [&str; 3] = ["medium", "high", "xhigh"];
const QUEUE_LIMITS: [&str; 5] = ["max_running", "tests", "perf", "background", "service"];
const QUEUE_LIMIT_MAX: i64 = 16;
/// Each terminal limit and its allowed range. A value out of range is refused
/// in a `PUT` and clamped when it comes from config.
pub const TERMINAL_LIMITS: [(&str, i64, i64); 4] = [
    ("per_user", 1, 256),
    ("per_session", 1, 256),
    ("global", 1, 256),
    ("max_attach_seconds", 60, 86_400),
];
const AGENT_NAME_MAX_CHARS: usize = 32;

pub const DEFAULT_NAME_PATTERN: &str = "{repo_short}-{number}";
pub const DEFAULT_MESSAGE_TEMPLATE: &str = "Work ticket {ticket} in {repo}: {title}\n{url}\n\nYou already hold the claim on {ticket}. Run `sm ticket {number} --setup-worktree` and work in the worktree it prints, then follow this repo's CLAUDE.md.";

/// The full settings object with nothing set.
pub fn defaults() -> Value {
    let workspace = |name: &str| {
        expand_home(&format!("~/projects/{name}"))
            .to_string_lossy()
            .into_owned()
    };
    json!({
        "new_agent": {
            "provider": "claude",
            "claude": { "model": null, "effort": null },
            "codex": { "model": null, "effort": null },
            "workspaces": [
                workspace("fractal-algo-rust"),
                workspace("session-manager"),
                workspace("codex-fork"),
            ],
            "name_pattern": DEFAULT_NAME_PATTERN,
            "repo_short": {
                "rajeshgoli/fractal-algo-rust": "far",
                "rajeshgoli/session-manager": "sm",
            },
            "message_template": DEFAULT_MESSAGE_TEMPLATE,
        },
        "queue_limits": {
            "max_running": null,
            "tests": null,
            "perf": null,
            "background": null,
            "service": null,
        },
        "terminal_limits": {
            "per_user": null,
            "per_session": null,
            "global": null,
            "max_attach_seconds": null,
        },
        "reviews": {
            "reviewer": { "kind": "github_codex" },
            "skip_meter_percent": 95,
        },
    })
}

/// The effective settings object. `stored` maps each key to
/// `{value, updated_at}`. A stored key that no longer validates (a
/// hand-edited store) falls back to its defaults rather than failing reads.
pub fn effective(stored: Option<&Value>) -> Value {
    let mut settings = defaults();
    for key in KEYS {
        let Some(owner) = stored_value(stored, key) else {
            continue;
        };
        let merged = overlay(&settings[key], owner, false);
        if validate_key(key, &merged).is_ok() {
            settings[key] = merged;
        }
    }
    settings
}

/// The owner's stored values for one key.
pub fn stored_value<'a>(stored: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    stored?
        .get(key)?
        .get("value")
        .filter(|value| value.is_object())
}

/// Apply a `PUT /client/settings` body to the stored values. Returns the new
/// stored value of each key the body touches, or an error naming the field.
pub fn apply_patch(
    stored: Option<&Value>,
    patch: &Value,
) -> Result<BTreeMap<String, Value>, String> {
    let patch = patch
        .as_object()
        .ok_or_else(|| "body must be a JSON object".to_owned())?;
    let defaults = defaults();
    let mut changed = BTreeMap::new();
    for (key, value) in patch {
        let default = defaults
            .get(key.as_str())
            .ok_or_else(|| format!("unknown field {key}"))?;
        let mut owner = stored_value(stored, key)
            .cloned()
            .unwrap_or_else(|| json!({}));
        if value.is_null() {
            owner = json!({});
        } else {
            let value = value
                .as_object()
                .ok_or_else(|| format!("{key} must be an object"))?;
            merge_patch(default, owner_object(&mut owner), value, key)?;
        }
        validate_key(key, &overlay(default, &owner, false))?;
        changed.insert(key.clone(), owner);
    }
    Ok(changed)
}

/// `claude` settings for the Claude provider, `codex` for Codex.
fn provider_settings_key(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("claude"),
        "codex-fork" => Some("codex"),
        _ => None,
    }
}

fn owner_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().expect("object")
}

/// Merge `patch` into the owner's values at `path`, shaped by `default`.
/// Unknown fields are refused; `null` removes the owner's value.
fn merge_patch(
    default: &Value,
    owner: &mut Map<String, Value>,
    patch: &Map<String, Value>,
    path: &str,
) -> Result<(), String> {
    for (key, value) in patch {
        let field = format!("{path}.{key}");
        let default = default
            .get(key.as_str())
            .ok_or_else(|| format!("unknown field {field}"))?;
        if value.is_null() {
            owner.remove(key);
        } else if default.is_object() && !WHOLE_VALUES.contains(&key.as_str()) {
            let value = value
                .as_object()
                .ok_or_else(|| format!("{field} must be an object"))?;
            let nested = owner_object(owner.entry(key.clone()).or_insert_with(|| json!({})));
            merge_patch(default, nested, value, &field)?;
            if nested.is_empty() {
                owner.remove(key);
            }
        } else {
            owner.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

/// `default` with the owner's values laid over it. `whole` replaces rather
/// than merges.
fn overlay(default: &Value, owner: &Value, whole: bool) -> Value {
    match (default, owner) {
        (Value::Object(default), Value::Object(owner)) if !whole => {
            let mut merged = default.clone();
            for (key, value) in owner {
                let next = match default.get(key) {
                    Some(base) => overlay(base, value, WHOLE_VALUES.contains(&key.as_str())),
                    None => value.clone(),
                };
                merged.insert(key.clone(), next);
            }
            Value::Object(merged)
        }
        _ => owner.clone(),
    }
}

fn validate_key(key: &str, value: &Value) -> Result<(), String> {
    match key {
        "new_agent" => validate_new_agent(value),
        "queue_limits" => validate_queue_limits(value),
        "terminal_limits" => validate_terminal_limits(value),
        "reviews" => {
            if value["reviewer"] != json!({"kind": "github_codex"}) {
                return Err("reviews.reviewer must be {\"kind\":\"github_codex\"}".to_owned());
            }
            if !value["skip_meter_percent"]
                .as_i64()
                .is_some_and(|percent| (50..=100).contains(&percent))
            {
                return Err("reviews.skip_meter_percent must be 50–100".to_owned());
            }
            Ok(())
        }
        other => Err(format!("unknown field {other}")),
    }
}

fn validate_new_agent(value: &Value) -> Result<(), String> {
    let provider = value["provider"].as_str();
    if provider_settings_key(provider.unwrap_or_default()).is_none() {
        return Err("new_agent.provider must be claude or codex-fork".to_owned());
    }
    for (key, efforts) in [
        ("claude", &CLAUDE_EFFORTS[..]),
        ("codex", &CODEX_EFFORTS[..]),
    ] {
        let model = &value[key]["model"];
        if !model.is_null() && model.as_str().is_none_or(|model| model.trim().is_empty()) {
            return Err(format!(
                "new_agent.{key}.model must be a non-empty model name or null"
            ));
        }
        let effort = &value[key]["effort"];
        if !effort.is_null()
            && !effort
                .as_str()
                .is_some_and(|effort| efforts.contains(&effort))
        {
            return Err(format!(
                "new_agent.{key}.effort must be one of {}, or null",
                efforts.join(", ")
            ));
        }
    }
    let workspaces_ok = value["workspaces"].as_array().is_some_and(|workspaces| {
        workspaces.iter().all(|path| {
            path.as_str()
                .is_some_and(|path| Path::new(path).is_absolute())
        })
    });
    if !workspaces_ok {
        return Err("new_agent.workspaces must be a list of absolute paths".to_owned());
    }
    for field in ["name_pattern", "message_template"] {
        let template = value[field]
            .as_str()
            .filter(|template| !template.trim().is_empty())
            .ok_or_else(|| format!("new_agent.{field} must be a non-empty string"))?;
        if let Some((_, name)) = placeholders(template)
            .into_iter()
            .find(|(_, name)| !PLACEHOLDERS.contains(name))
        {
            return Err(format!(
                "new_agent.{field} uses unknown placeholder {{{name}}}; use {}",
                PLACEHOLDERS.map(|name| format!("{{{name}}}")).join(", ")
            ));
        }
    }
    let repo_short = value["repo_short"]
        .as_object()
        .ok_or_else(|| "new_agent.repo_short must be an object of repo to short name".to_owned())?;
    let mut seen = BTreeSet::new();
    for (repo, short) in repo_short {
        let short = short
            .as_str()
            .filter(|short| is_repo_short(short))
            .ok_or_else(|| {
                format!("new_agent.repo_short.{repo} must be 1 to 12 characters of a-z and 0-9")
            })?;
        if !seen.insert(short) {
            return Err(format!(
                "new_agent.repo_short.{repo}: {short} is already another repo's short name"
            ));
        }
    }
    Ok(())
}

fn is_repo_short(short: &str) -> bool {
    (1..=12).contains(&short.len())
        && short
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn validate_queue_limits(value: &Value) -> Result<(), String> {
    for limit in QUEUE_LIMITS {
        let value = &value[limit];
        if !value.is_null()
            && !value
                .as_i64()
                .is_some_and(|value| (0..=QUEUE_LIMIT_MAX).contains(&value))
        {
            return Err(format!(
                "queue_limits.{limit} must be an integer from 0 to {QUEUE_LIMIT_MAX}, or null"
            ));
        }
    }
    Ok(())
}

fn validate_terminal_limits(value: &Value) -> Result<(), String> {
    for (limit, min, max) in TERMINAL_LIMITS {
        let value = &value[limit];
        if !value.is_null()
            && !value
                .as_i64()
                .is_some_and(|value| (min..=max).contains(&value))
        {
            return Err(format!(
                "terminal_limits.{limit} must be an integer from {min} to {max}, or null"
            ));
        }
    }
    Ok(())
}

/// Each `{name}` in `template`, with its byte offset. Braces around
/// anything but letters, digits and `_` are plain text.
fn placeholders(template: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut rest = 0;
    while let Some(open) = template[rest..].find('{').map(|at| rest + at) {
        let inner = &template[open + 1..];
        let end = inner
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(inner.len());
        if end > 0 && inner[end..].starts_with('}') {
            found.push((open, &inner[..end]));
            rest = open + end + 2;
        } else {
            rest = open + 1;
        }
    }
    found
}

/// The effective settings, typed. Built from an object `effective` returned.
#[derive(Debug, Clone, Deserialize)]
pub struct OwnerSettings {
    pub new_agent: NewAgentSettings,
    pub queue_limits: QueueLimits,
    pub terminal_limits: TerminalLimitOverrides,
}

impl OwnerSettings {
    pub fn from_effective(value: &Value) -> anyhow::Result<Self> {
        Ok(serde_json::from_value(value.clone())?)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewAgentSettings {
    pub provider: String,
    pub claude: ProviderDefaults,
    pub codex: ProviderDefaults,
    pub workspaces: Vec<String>,
    pub name_pattern: String,
    pub repo_short: BTreeMap<String, String>,
    pub message_template: String,
}

/// `None` leaves the choice to the provider.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ProviderDefaults {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// A board ticket New agent or Start fills a name and first message from.
#[derive(Debug, Clone, Copy)]
pub struct Ticket<'a> {
    /// `owner/name`.
    pub repo: &'a str,
    pub number: i64,
    pub title: &'a str,
    pub url: &'a str,
}

impl NewAgentSettings {
    /// The model and effort of the default provider.
    pub fn provider_defaults(&self) -> &ProviderDefaults {
        match self.provider.as_str() {
            "codex-fork" => &self.codex,
            _ => &self.claude,
        }
    }

    /// The agent name `name_pattern` renders, made a valid name.
    pub fn agent_name(&self, ticket: Ticket<'_>) -> String {
        normalize_agent_name(&self.render(&self.name_pattern, ticket))
    }

    /// The first message `message_template` renders.
    pub fn brief(&self, ticket: Ticket<'_>) -> String {
        self.render(&self.message_template, ticket)
    }

    fn render(&self, template: &str, ticket: Ticket<'_>) -> String {
        let repo_name = ticket.repo.rsplit('/').next().unwrap_or(ticket.repo);
        let value = |name: &str| -> Option<String> {
            Some(match name {
                "ticket" => format!("#{}", ticket.number),
                "number" => ticket.number.to_string(),
                "repo" => ticket.repo.to_owned(),
                "repo_name" => repo_name.to_owned(),
                "repo_short" => self
                    .repo_short
                    .get(ticket.repo)
                    .cloned()
                    .unwrap_or_else(|| repo_name.to_owned()),
                "title" => ticket.title.to_owned(),
                "url" => ticket.url.to_owned(),
                _ => return None,
            })
        };
        let mut rendered = String::with_capacity(template.len());
        let mut copied = 0;
        for (at, name) in placeholders(template) {
            if let Some(text) = value(name) {
                rendered.push_str(&template[copied..at]);
                rendered.push_str(&text);
                copied = at + name.len() + 2;
            }
        }
        rendered.push_str(&template[copied..]);
        rendered
    }
}

/// Lower-case, every run of characters outside `[a-z0-9-]` replaced by `-`,
/// cut to 32 characters.
pub fn normalize_agent_name(raw: &str) -> String {
    let mut name = String::with_capacity(raw.len());
    let mut in_run = false;
    for c in raw.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            name.push(c);
            in_run = false;
        } else if !in_run {
            name.push('-');
            in_run = true;
        }
    }
    name.chars().take(AGENT_NAME_MAX_CHARS).collect()
}

/// Slot limits the owner set; `None` keeps config's value.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub struct QueueLimits {
    pub max_running: Option<i64>,
    pub tests: Option<i64>,
    pub perf: Option<i64>,
    pub background: Option<i64>,
    pub service: Option<i64>,
}

impl QueueLimits {
    /// `policy` with these limits laid over it.
    pub fn apply(self, mut policy: QueueAdmissionPolicy) -> QueueAdmissionPolicy {
        let slots = |limit: Option<i64>, current: usize| {
            limit.map_or(current, |limit| limit.clamp(0, QUEUE_LIMIT_MAX) as usize)
        };
        if let Some(limit) = self.max_running {
            policy.max_running_jobs = limit.clamp(0, QUEUE_LIMIT_MAX);
        }
        policy.tests_max_concurrent = slots(self.tests, policy.tests_max_concurrent);
        policy.perf_max_concurrent = slots(self.perf, policy.perf_max_concurrent);
        policy.background_max_concurrent = slots(self.background, policy.background_max_concurrent);
        policy.service_max_concurrent = slots(self.service, policy.service_max_concurrent);
        policy
    }
}

/// Config's admission policy with the owner's queue limits laid over it.
/// `settings` is an effective settings object.
pub fn queue_admission_policy(config: &AppConfig, settings: &Value) -> QueueAdmissionPolicy {
    let limits = OwnerSettings::from_effective(settings)
        .map(|settings| settings.queue_limits)
        .unwrap_or_default();
    limits.apply(config.queue_admission_policy())
}

/// Terminal attach limits the owner set; `None` keeps config's value.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub struct TerminalLimitOverrides {
    pub per_user: Option<i64>,
    pub per_session: Option<i64>,
    pub global: Option<i64>,
    pub max_attach_seconds: Option<i64>,
}

/// The terminal attach limits in force: config's values, clamped to
/// `TERMINAL_LIMITS`, with the owner's overrides laid over them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalLimits {
    pub per_user: usize,
    pub per_session: usize,
    pub global: usize,
    pub max_attach_seconds: u64,
}

impl TerminalLimits {
    /// Config's values alone. A zero in config means the compiled default.
    pub fn from_config(config: &AppConfig) -> Self {
        let defaults = crate::config::MobileTerminalConfig::default();
        let terminal = &config.mobile_terminal;
        let pick = |value: i64, default: i64, index: usize| {
            let (_, min, max) = TERMINAL_LIMITS[index];
            if value == 0 { default } else { value }.clamp(min, max)
        };
        Self {
            per_user: pick(
                terminal.max_concurrent_attaches_per_user as i64,
                defaults.max_concurrent_attaches_per_user as i64,
                0,
            ) as usize,
            per_session: pick(
                terminal.max_concurrent_attaches_per_session as i64,
                defaults.max_concurrent_attaches_per_session as i64,
                1,
            ) as usize,
            global: pick(
                terminal.max_concurrent_attaches_global as i64,
                defaults.max_concurrent_attaches_global as i64,
                2,
            ) as usize,
            max_attach_seconds: pick(
                terminal.max_attach_seconds as i64,
                defaults.max_attach_seconds as i64,
                3,
            ) as u64,
        }
    }

    /// `self` with the owner's overrides laid over it.
    pub fn apply(self, owner: TerminalLimitOverrides) -> Self {
        let pick = |value: Option<i64>, current: i64, index: usize| {
            let (_, min, max) = TERMINAL_LIMITS[index];
            value.map_or(current, |value| value.clamp(min, max))
        };
        Self {
            per_user: pick(owner.per_user, self.per_user as i64, 0) as usize,
            per_session: pick(owner.per_session, self.per_session as i64, 1) as usize,
            global: pick(owner.global, self.global as i64, 2) as usize,
            max_attach_seconds: pick(owner.max_attach_seconds, self.max_attach_seconds as i64, 3)
                as u64,
        }
    }

    /// The config values as a JSON object, for the settings pages to show
    /// what Reset restores.
    pub fn to_json(self) -> Value {
        json!({
            "per_user": self.per_user,
            "per_session": self.per_session,
            "global": self.global,
            "max_attach_seconds": self.max_attach_seconds,
        })
    }
}

/// Config's terminal limits with the owner's overrides laid over them.
/// `settings` is an effective settings object.
pub fn terminal_limits(config: &AppConfig, settings: &Value) -> TerminalLimits {
    let owner = OwnerSettings::from_effective(settings)
        .map(|settings| settings.terminal_limits)
        .unwrap_or_default();
    TerminalLimits::from_config(config).apply(owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(stored: Option<&Value>) -> OwnerSettings {
        OwnerSettings::from_effective(&effective(stored)).unwrap()
    }

    /// `previous` with `changed` written, as the store writes it.
    fn stored(previous: Option<&Value>, changed: BTreeMap<String, Value>) -> Value {
        let mut stored = previous.cloned().unwrap_or_else(|| json!({}));
        for (key, value) in changed {
            stored[key] = json!({ "value": value, "updated_at": "t" });
        }
        stored
    }

    const TICKET: Ticket<'static> = Ticket {
        repo: "rajeshgoli/session-manager",
        number: 1706,
        title: "Board Start preselects Fable",
        url: "https://github.com/rajeshgoli/session-manager/issues/1706",
    };

    #[test]
    fn defaults_name_sm_1706_and_leave_model_and_effort_to_the_provider() {
        let settings = settings(None);
        assert_eq!(settings.new_agent.agent_name(TICKET), "sm-1706");
        assert_eq!(
            settings.new_agent.brief(TICKET),
            "Work ticket #1706 in rajeshgoli/session-manager: Board Start preselects Fable\n\
             https://github.com/rajeshgoli/session-manager/issues/1706\n\n\
             You already hold the claim on #1706. Run `sm ticket 1706 --setup-worktree` and \
             work in the worktree it prints, then follow this repo's CLAUDE.md."
        );
        assert_eq!(settings.new_agent.provider, "claude");
        assert_eq!(
            settings.new_agent.provider_defaults(),
            &ProviderDefaults::default()
        );
        assert!(settings
            .new_agent
            .workspaces
            .iter()
            .all(|path| Path::new(path).is_absolute()));
        // A repo with no short name uses its name.
        let other = Ticket {
            repo: "rajeshgoli/codex-fork",
            ..TICKET
        };
        assert_eq!(settings.new_agent.agent_name(other), "codex-fork-1706");
    }

    #[test]
    fn a_rendered_name_is_lower_cased_dashed_and_cut_to_32_characters() {
        let changed = apply_patch(
            None,
            &json!({"new_agent": {"name_pattern": "{repo_short} {title}!"}}),
        )
        .unwrap();
        let settings = settings(Some(&stored(None, changed)));
        assert_eq!(
            settings.new_agent.agent_name(TICKET),
            "sm-board-start-preselects-fable-"
        );
        assert_eq!(normalize_agent_name("A  b__c--D"), "a-b-c--d");
        // Braces around a non-placeholder are plain text.
        assert_eq!(placeholders("{ x } {a-b} {ok}"), vec![(12, "ok")]);
    }

    #[test]
    fn put_merges_nested_fields_and_null_restores_the_default() {
        let first = stored(None, apply_patch(None,
                &json!({"new_agent": {"provider": "codex-fork", "codex": {"model": "gpt-5.5", "effort": "xhigh"}}}),
            )
            .unwrap(),
        );
        let second = stored(Some(&first), apply_patch(Some(&first),
                &json!({"new_agent": {"codex": {"effort": null}}, "queue_limits": {"background": 3}}),
            )
            .unwrap(),
        );
        let value = effective(Some(&second));
        assert_eq!(value["new_agent"]["provider"], "codex-fork");
        assert_eq!(
            value["new_agent"]["codex"],
            json!({"model": "gpt-5.5", "effort": null})
        );
        assert_eq!(value["queue_limits"]["background"], 3);
        assert_eq!(value["queue_limits"]["tests"], Value::Null);
        // repo_short is replaced whole, not merged.
        let third = stored(
            Some(&second),
            apply_patch(
                Some(&second),
                &json!({"new_agent": {"repo_short": {"rajeshgoli/codex-fork": "cf"}}}),
            )
            .unwrap(),
        );
        assert_eq!(
            effective(Some(&third))["new_agent"]["repo_short"],
            json!({"rajeshgoli/codex-fork": "cf"})
        );
        // null on a whole key resets every field of it.
        let fourth = stored(
            Some(&third),
            apply_patch(Some(&third), &json!({"new_agent": null})).unwrap(),
        );
        assert_eq!(
            effective(Some(&fourth))["new_agent"],
            defaults()["new_agent"]
        );
        assert_eq!(effective(Some(&fourth))["queue_limits"]["background"], 3);
    }

    #[test]
    fn put_refuses_bad_values_naming_the_field() {
        for (patch, error) in [
            (json!({"new_agent": {"claude": {"effort": "xhigh"}}}), "new_agent.claude.effort must be one of low, medium, high, max, or null"),
            (json!({"new_agent": {"codex": {"effort": "low"}}}), "new_agent.codex.effort must be one of medium, high, xhigh, or null"),
            (json!({"new_agent": {"claude": {"model": " "}}}), "new_agent.claude.model must be a non-empty model name or null"),
            (json!({"new_agent": {"provider": "codex"}}), "new_agent.provider must be claude or codex-fork"),
            (json!({"new_agent": {"workspaces": ["~/projects/x"]}}), "new_agent.workspaces must be a list of absolute paths"),
            (json!({"new_agent": {"name_pattern": "{repo}-{branch}"}}), "new_agent.name_pattern uses unknown placeholder {branch}; use {ticket}, {number}, {repo}, {repo_name}, {repo_short}, {title}, {url}"),
            (json!({"new_agent": {"message_template": ""}}), "new_agent.message_template must be a non-empty string"),
            (json!({"new_agent": {"repo_short": {"a/b": "Far"}}}), "new_agent.repo_short.a/b must be 1 to 12 characters of a-z and 0-9"),
            (json!({"new_agent": {"repo_short": {"a/b": "x", "c/d": "x"}}}), "new_agent.repo_short.c/d: x is already another repo's short name"),
            (json!({"new_agent": {"colour": "red"}}), "unknown field new_agent.colour"),
            (json!({"theme": "dark"}), "unknown field theme"),
            (json!({"queue_limits": {"tests": 17}}), "queue_limits.tests must be an integer from 0 to 16, or null"),
            (json!({"queue_limits": {"perf": 1.5}}), "queue_limits.perf must be an integer from 0 to 16, or null"),
            (json!({"queue_limits": 3}), "queue_limits must be an object"),
            (json!([]), "body must be a JSON object"),
        ] {
            assert_eq!(apply_patch(None, &patch).unwrap_err(), error, "{patch}");
        }
    }

    #[test]
    fn queue_limits_overlay_only_the_limits_the_owner_set() {
        let policy = QueueLimits {
            background: Some(3),
            max_running: Some(4),
            ..QueueLimits::default()
        }
        .apply(QueueAdmissionPolicy::default());
        assert_eq!(policy.background_max_concurrent, 3);
        assert_eq!(policy.max_running_jobs, 4);
        assert_eq!(
            policy.tests_max_concurrent,
            QueueAdmissionPolicy::default().tests_max_concurrent
        );
    }
}

#[cfg(test)]
mod terminal_limit_tests {
    use super::*;

    #[test]
    fn config_zero_means_default_and_values_clamp_to_the_allowed_range() {
        let mut config = AppConfig::default();
        config.mobile_terminal.max_concurrent_attaches_per_user = 0;
        config.mobile_terminal.max_concurrent_attaches_per_session = 1000;
        config.mobile_terminal.max_concurrent_attaches_global = 7;
        config.mobile_terminal.max_attach_seconds = 5;
        let limits = TerminalLimits::from_config(&config);
        assert_eq!(
            limits,
            TerminalLimits {
                per_user: 100,
                per_session: 256,
                global: 7,
                max_attach_seconds: 60,
            }
        );
    }

    #[test]
    fn owner_values_override_only_what_the_owner_set() {
        let config = AppConfig::default();
        let settings = effective(Some(&json!({
            "terminal_limits": {"value": {"per_session": 1}, "updated_at": "x"}
        })));
        let limits = terminal_limits(&config, &settings);
        assert_eq!(
            (
                limits.per_user,
                limits.per_session,
                limits.global,
                limits.max_attach_seconds
            ),
            (100, 1, 100, 86_400)
        );
        let refused = apply_patch(
            None,
            &json!({"terminal_limits": {"max_attach_seconds": 59}}),
        );
        assert_eq!(
            refused.unwrap_err(),
            "terminal_limits.max_attach_seconds must be an integer from 60 to 86400, or null"
        );
    }
}
