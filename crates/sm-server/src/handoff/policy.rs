//! The handoff policy: the stored default policy, per-agent overrides, their
//! resolution into an effective policy, the handoff state a session carries,
//! and the texts sm sends (spec Appendices A, B, D and I.2).
//!
//! Everything here is pure. `sessions.rs` owns persistence and delivery.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// Queue category of asks, reminders and withdrawals. A completed handoff
/// cancels leftovers by this category.
pub const MESSAGE_CATEGORY: &str = "context_handoff";
/// Asks arrive at the agent's next turn boundary, never mid-tool-call.
pub const DELIVERY_MODE: &str = "important";

/// Top-level key of the stored default policy in the session store.
pub const DEFAULTS_KEY: &str = "handoff_defaults";
/// Session-record key of a per-agent override.
pub const OVERRIDE_KEY: &str = "handoff_policy_override";
/// Session-record key of the handoff state.
pub const STATE_KEY: &str = "handoff";

pub const WITHDRAWN_TEXT: &str =
    "[sm context management] The handoff request is withdrawn. Continue working.";

const DEFAULT_THRESHOLD_PERCENT: f64 = 35.0;
const DEFAULT_REVIEW_FLOOR_PERCENT: f64 = 20.0;
const DEFAULT_REMINDER_PERCENT: f64 = 50.0;
const CODEX_THRESHOLD_PERCENT: f64 = 80.0;
const CODEX_REMINDER_PERCENT: f64 = 90.0;
const CODEX_REVIEW_FLOOR_PERCENT: f64 = 50.0;
/// When the stored reminder is not above the ask, the reminder comes this many
/// points after the ask, never later than the cap (spec Appendix C.2).
const REMINDER_STEP_PERCENT: f64 = 10.0;
const REMINDER_CAP_PERCENT: f64 = 90.0;
const DEFAULT_FIELDS: [&str; 7] = [
    "providers",
    "threshold_percent",
    "ask_on_codex_review",
    "ask_on_doc_review",
    "review_floor_percent",
    "reminder_percent",
    "provider_thresholds",
];

/// The default policy (Appendix B). Starting values apply until the first
/// successful `PUT /handoff-defaults` writes the full object.
#[derive(Debug, Clone, PartialEq)]
pub struct HandoffDefaults {
    pub providers: BTreeMap<String, bool>,
    pub provider_thresholds: BTreeMap<String, ProviderThresholds>,
    pub window_tokens: BTreeMap<String, i64>,
    pub threshold_percent: f64,
    pub ask_on_codex_review: bool,
    pub ask_on_doc_review: bool,
    pub review_floor_percent: f64,
    pub reminder_percent: f64,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderThresholds {
    pub threshold_percent: f64,
    pub reminder_percent: f64,
    pub review_floor_percent: f64,
}

impl ProviderThresholds {
    fn to_json(&self) -> Value {
        json!({
            "threshold_percent": percent_json(self.threshold_percent),
            "reminder_percent": percent_json(self.reminder_percent),
            "review_floor_percent": percent_json(self.review_floor_percent),
        })
    }
}

impl Default for HandoffDefaults {
    fn default() -> Self {
        Self {
            providers: BTreeMap::from([
                ("claude".to_owned(), true),
                ("opencode".to_owned(), true),
                ("codex-fork".to_owned(), false),
                ("codex-app".to_owned(), false),
            ]),
            provider_thresholds: BTreeMap::from([
                (
                    "opencode".to_owned(),
                    ProviderThresholds {
                        threshold_percent: 75.0,
                        reminder_percent: 85.0,
                        review_floor_percent: 50.0,
                    },
                ),
                (
                    "claude".to_owned(),
                    ProviderThresholds {
                        threshold_percent: DEFAULT_THRESHOLD_PERCENT,
                        reminder_percent: DEFAULT_REMINDER_PERCENT,
                        review_floor_percent: DEFAULT_REVIEW_FLOOR_PERCENT,
                    },
                ),
                (
                    "codex-fork".to_owned(),
                    ProviderThresholds {
                        threshold_percent: CODEX_THRESHOLD_PERCENT,
                        reminder_percent: CODEX_REMINDER_PERCENT,
                        review_floor_percent: CODEX_REVIEW_FLOOR_PERCENT,
                    },
                ),
            ]),
            window_tokens: BTreeMap::from([
                ("claude".to_owned(), 1_000_000),
                ("opencode".to_owned(), 200_000),
                ("codex-fork".to_owned(), 258_400),
            ]),
            threshold_percent: DEFAULT_THRESHOLD_PERCENT,
            ask_on_codex_review: true,
            ask_on_doc_review: true,
            review_floor_percent: DEFAULT_REVIEW_FLOOR_PERCENT,
            reminder_percent: DEFAULT_REMINDER_PERCENT,
            updated_at: None,
        }
    }
}

impl HandoffDefaults {
    /// The stored object, with starting values filling any field that is
    /// absent. A stored field that fails validation also falls back, so a
    /// hand-edited store can never switch the policy into a nonsense state.
    pub fn from_stored(stored: Option<&Value>) -> Self {
        let mut defaults = Self::default();
        let Some(stored) = stored.and_then(Value::as_object) else {
            return defaults;
        };
        let mut patch = stored.clone();
        patch.retain(|key, _| DEFAULT_FIELDS.contains(&key.as_str()));
        // Each field is applied on its own so one bad field keeps the rest.
        for (key, value) in patch {
            if let Ok(merged) = defaults.merged(&json!({ key: value })) {
                defaults = merged;
            }
        }
        if let Some(thresholds) = stored.get("provider_thresholds") {
            if let Ok(merged) = defaults.merged(&json!({ "provider_thresholds": thresholds })) {
                defaults = merged;
            }
        }
        if let Some(windows) = stored.get("window_tokens").and_then(Value::as_object) {
            for (provider, tokens) in windows {
                if let Some(tokens) = tokens.as_i64().filter(|tokens| *tokens > 0) {
                    defaults.window_tokens.insert(provider.clone(), tokens);
                }
            }
        }
        // Legacy stored flat fields predate provider thresholds. Their values
        // belong to Claude; Codex keeps its own starting values.
        if !stored.contains_key("provider_thresholds") {
            defaults.sync_claude_thresholds();
        }
        defaults.updated_at = stored
            .get("updated_at")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        defaults
    }

    /// Merge a `PUT /handoff-defaults` body. `providers` merges per provider.
    /// The error names the offending field.
    pub fn merged(&self, patch: &Value) -> Result<Self, String> {
        let patch = patch
            .as_object()
            .ok_or_else(|| "body must be a JSON object".to_owned())?;
        let mut next = self.clone();
        let explicit_claude = patch
            .get("provider_thresholds")
            .and_then(Value::as_object)
            .and_then(|providers| providers.get("claude"))
            .and_then(Value::as_object);
        for (key, value) in patch {
            match key.as_str() {
                "providers" => {
                    let providers = value
                        .as_object()
                        .ok_or_else(|| "providers must be an object of booleans".to_owned())?;
                    for (provider, enabled) in providers {
                        let enabled = enabled
                            .as_bool()
                            .ok_or_else(|| format!("providers.{provider} must be true or false"))?;
                        next.providers.insert(provider.clone(), enabled);
                    }
                }
                "threshold_percent" => {
                    let parsed = percent_field(key, value, false)?;
                    if !explicit_claude.is_some_and(|fields| fields.contains_key(key)) {
                        next.threshold_percent = parsed;
                        next.sync_claude_thresholds();
                    }
                }
                "review_floor_percent" => {
                    let parsed = percent_field(key, value, true)?;
                    if !explicit_claude.is_some_and(|fields| fields.contains_key(key)) {
                        next.review_floor_percent = parsed;
                        next.sync_claude_thresholds();
                    }
                }
                "reminder_percent" => {
                    let parsed = percent_field(key, value, false)?;
                    if !explicit_claude.is_some_and(|fields| fields.contains_key(key)) {
                        next.reminder_percent = parsed;
                        next.sync_claude_thresholds();
                    }
                }
                "provider_thresholds" => {
                    let providers = value
                        .as_object()
                        .ok_or_else(|| "provider_thresholds must be an object".to_owned())?;
                    for (provider, fields) in providers {
                        let fields = fields.as_object().ok_or_else(|| {
                            format!("provider_thresholds.{provider} must be an object")
                        })?;
                        let mut thresholds = next.thresholds(provider);
                        for (field, value) in fields {
                            let name = format!("provider_thresholds.{provider}.{field}");
                            match field.as_str() {
                                "threshold_percent" => {
                                    thresholds.threshold_percent =
                                        percent_field(&name, value, false)?
                                }
                                "reminder_percent" => {
                                    thresholds.reminder_percent =
                                        percent_field(&name, value, false)?
                                }
                                "review_floor_percent" => {
                                    thresholds.review_floor_percent =
                                        percent_field(&name, value, true)?
                                }
                                _ => return Err(format!("unknown field {name}")),
                            }
                        }
                        next.provider_thresholds
                            .insert(provider.clone(), thresholds);
                    }
                    next.sync_flat_claude();
                }
                "ask_on_codex_review" => next.ask_on_codex_review = bool_field(key, value)?,
                "ask_on_doc_review" => next.ask_on_doc_review = bool_field(key, value)?,
                "window_tokens" => {}
                // Read-only; echoing a GET body back is harmless.
                "updated_at" => {}
                other => return Err(format!("unknown field {other}")),
            }
        }
        Ok(next)
    }

    pub fn provider_enabled(&self, provider: &str) -> bool {
        self.providers.get(provider).copied().unwrap_or(false)
    }

    pub fn thresholds(&self, provider: &str) -> ProviderThresholds {
        self.provider_thresholds
            .get(provider)
            .cloned()
            .unwrap_or_else(|| self.provider_thresholds["claude"].clone())
    }

    fn sync_claude_thresholds(&mut self) {
        self.provider_thresholds.insert(
            "claude".to_owned(),
            ProviderThresholds {
                threshold_percent: self.threshold_percent,
                reminder_percent: self.reminder_percent,
                review_floor_percent: self.review_floor_percent,
            },
        );
    }

    fn sync_flat_claude(&mut self) {
        let claude = self.thresholds("claude");
        self.threshold_percent = claude.threshold_percent;
        self.reminder_percent = claude.reminder_percent;
        self.review_floor_percent = claude.review_floor_percent;
    }

    pub fn to_json(&self) -> Value {
        json!({
            "providers": self.providers,
            "provider_thresholds": self.provider_thresholds.iter().map(|(key, value)|
                (key.clone(), value.to_json())).collect::<BTreeMap<_, _>>(),
            "window_tokens": self.window_tokens,
            "threshold_percent": percent_json(self.threshold_percent),
            "ask_on_codex_review": self.ask_on_codex_review,
            "ask_on_doc_review": self.ask_on_doc_review,
            "review_floor_percent": percent_json(self.review_floor_percent),
            "reminder_percent": percent_json(self.reminder_percent),
            "updated_at": self.updated_at,
        })
    }
}

/// A per-agent override (Appendix A). `None` fields inherit the default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HandoffOverride {
    pub enabled: Option<bool>,
    pub threshold_percent: Option<f64>,
    pub set_at: Option<String>,
}

impl HandoffOverride {
    pub fn from_stored(stored: Option<&Value>) -> Option<Self> {
        let stored = stored.and_then(Value::as_object)?;
        Some(Self {
            enabled: stored.get("enabled").and_then(Value::as_bool),
            threshold_percent: stored
                .get("threshold_percent")
                .and_then(Value::as_f64)
                .filter(|value| valid_percent(*value, false)),
            set_at: stored
                .get("set_at")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }

    pub fn is_active(&self) -> bool {
        self.enabled.is_some() || self.threshold_percent.is_some()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "threshold_percent": self.threshold_percent.map(percent_json),
            "set_at": self.set_at,
        })
    }
}

/// One field of a policy update: left alone, reset to inherit, or set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldChange<T> {
    Unchanged,
    Inherit,
    Set(T),
}

/// The body of `PUT /sessions/{id}/handoff-policy` (Appendix I.1).
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyUpdate {
    pub enabled: FieldChange<bool>,
    pub threshold_percent: FieldChange<f64>,
    pub use_default: bool,
    pub ask_now: bool,
}

impl PolicyUpdate {
    pub fn parse(body: &Value) -> Result<Self, String> {
        let body = body
            .as_object()
            .ok_or_else(|| "body must be a JSON object".to_owned())?;
        let mut update = Self {
            enabled: FieldChange::Unchanged,
            threshold_percent: FieldChange::Unchanged,
            use_default: false,
            ask_now: false,
        };
        for (key, value) in body {
            match key.as_str() {
                "enabled" => {
                    update.enabled = if value.is_null() {
                        FieldChange::Inherit
                    } else {
                        FieldChange::Set(bool_field(key, value)?)
                    };
                }
                "threshold_percent" => {
                    update.threshold_percent = if value.is_null() {
                        FieldChange::Inherit
                    } else {
                        FieldChange::Set(percent_field(key, value, false)?)
                    };
                }
                "use_default" => update.use_default = bool_field(key, value)?,
                "ask_now" => update.ask_now = bool_field(key, value)?,
                other => return Err(format!("unknown field {other}")),
            }
        }
        if update.use_default
            && (update.enabled != FieldChange::Unchanged
                || update.threshold_percent != FieldChange::Unchanged)
        {
            return Err("use_default cannot be combined with enabled or threshold_percent".into());
        }
        Ok(update)
    }

    pub fn changes_override(&self) -> bool {
        self.use_default
            || self.enabled != FieldChange::Unchanged
            || self.threshold_percent != FieldChange::Unchanged
    }

    /// The override after this update. `None` means no override is stored.
    pub fn apply(&self, current: Option<&HandoffOverride>, now: &str) -> Option<HandoffOverride> {
        if self.use_default {
            return None;
        }
        let mut next = current.cloned().unwrap_or_default();
        match self.enabled {
            FieldChange::Unchanged => {}
            FieldChange::Inherit => next.enabled = None,
            FieldChange::Set(value) => next.enabled = Some(value),
        }
        match self.threshold_percent {
            FieldChange::Unchanged => {}
            FieldChange::Inherit => next.threshold_percent = None,
            FieldChange::Set(value) => next.threshold_percent = Some(value),
        }
        next.set_at = Some(now.to_owned());
        next.is_active().then_some(next)
    }
}

/// The policy that applies to one session (Appendix B, "Effective policy").
#[derive(Debug, Clone, PartialEq)]
pub struct EffectivePolicy {
    pub enabled: bool,
    pub threshold_percent: f64,
    /// The stored reminder percent for the provider; see [`Self::reminder_at`].
    pub reminder_percent: f64,
    pub source: &'static str,
    pub has_gauge: bool,
}

pub fn effective_policy(
    defaults: &HandoffDefaults,
    provider: &str,
    override_: Option<&HandoffOverride>,
    has_gauge: bool,
) -> EffectivePolicy {
    effective_policy_with_ticket(defaults, provider, override_, None, has_gauge)
}

pub fn effective_policy_with_ticket(
    defaults: &HandoffDefaults,
    provider: &str,
    override_: Option<&HandoffOverride>,
    ticket: Option<&HandoffOverride>,
    has_gauge: bool,
) -> EffectivePolicy {
    let thresholds = defaults.thresholds(provider);
    let enabled = override_
        .and_then(|value| value.enabled)
        .or_else(|| ticket.and_then(|value| value.enabled))
        .unwrap_or_else(|| defaults.provider_enabled(provider));
    let threshold_percent = override_
        .and_then(|value| value.threshold_percent)
        .or_else(|| ticket.and_then(|value| value.threshold_percent))
        .unwrap_or(thresholds.threshold_percent);
    let reminder_percent = thresholds.reminder_percent;
    let source = if override_.is_some_and(HandoffOverride::is_active) {
        "override"
    } else if ticket.is_some_and(HandoffOverride::is_active) {
        "ticket"
    } else {
        "default"
    };
    EffectivePolicy {
        enabled,
        threshold_percent,
        reminder_percent,
        source,
        has_gauge,
    }
}

impl EffectivePolicy {
    /// The usage at which an agent asked at `asked_percent` is reminded: the
    /// stored reminder when it is above the ask, otherwise ten points after
    /// the ask capped at 90. `None` only for an ask at or past the cap.
    pub fn reminder_at(&self, asked_percent: f64) -> Option<f64> {
        if self.reminder_percent > asked_percent {
            Some(self.reminder_percent)
        } else if asked_percent < REMINDER_CAP_PERCENT {
            Some((asked_percent + REMINDER_STEP_PERCENT).min(REMINDER_CAP_PERCENT))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffPhase {
    Asked,
    Accepted,
    Spawning,
    Transferring,
    Done,
    Failed,
}

impl HandoffPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asked => "asked",
            Self::Accepted => "accepted",
            Self::Spawning => "spawning",
            Self::Transferring => "transferring",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffTrigger {
    Context,
    ReviewRequest,
    DocReview,
    Owner,
    Voluntary,
}

impl HandoffTrigger {
    /// Asks made when the agent requests a review: suggestions that the
    /// threshold ask supersedes, with no reminder of their own.
    pub fn is_review(self) -> bool {
        matches!(self, Self::ReviewRequest | Self::DocReview)
    }
}

/// The `handoff` key of a session record (Appendix A). Absent means the
/// agent is working and has never been asked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffRecord {
    pub state: HandoffPhase,
    pub trigger: HandoffTrigger,
    #[serde(default)]
    pub asked_at: Option<String>,
    #[serde(default)]
    pub asked_percent: Option<f64>,
    #[serde(default)]
    pub reminded_at: Option<String>,
    #[serde(default)]
    pub note: Option<Value>,
    #[serde(default)]
    pub accepted_at: Option<String>,
    #[serde(default)]
    pub successor_session_id: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub failed_at: Option<String>,
}

impl HandoffRecord {
    pub fn asked(trigger: HandoffTrigger, now: &str, percent: Option<f64>) -> Self {
        Self {
            state: HandoffPhase::Asked,
            trigger,
            asked_at: Some(now.to_owned()),
            asked_percent: percent,
            reminded_at: None,
            note: None,
            accepted_at: None,
            successor_session_id: None,
            error: None,
            failed_at: None,
        }
    }

    /// `None` when the key is absent or null. A record this build cannot
    /// parse still counts as present, so sm never re-asks over it.
    pub fn from_session(session: &Map<String, Value>) -> Option<Result<Self, ()>> {
        let value = session.get(STATE_KEY).filter(|value| !value.is_null())?;
        Some(serde_json::from_value(value.clone()).map_err(|_| ()))
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Why sm is asking (Appendix D.1, `<reason>`).
#[derive(Debug, Clone, PartialEq)]
pub enum AskReason<'a> {
    Context {
        percent: f64,
    },
    ReviewRequest {
        percent: Option<f64>,
        pr_number: i64,
    },
    DocReview {
        percent: Option<f64>,
        owner_name: &'a str,
        doc_title: &'a str,
    },
    Owner {
        owner_name: &'a str,
    },
}

impl AskReason<'_> {
    pub fn trigger(&self) -> HandoffTrigger {
        match self {
            Self::Context { .. } => HandoffTrigger::Context,
            Self::ReviewRequest { .. } => HandoffTrigger::ReviewRequest,
            Self::DocReview { .. } => HandoffTrigger::DocReview,
            Self::Owner { .. } => HandoffTrigger::Owner,
        }
    }

    pub fn percent(&self) -> Option<f64> {
        match self {
            Self::Context { percent } => Some(*percent),
            Self::ReviewRequest { percent, .. } | Self::DocReview { percent, .. } => *percent,
            Self::Owner { .. } => None,
        }
    }

    fn text(&self) -> String {
        match self {
            Self::Context { percent } => {
                format!("Your context is at {}%.", round_percent(*percent))
            }
            Self::ReviewRequest {
                percent: Some(percent),
                pr_number,
            } => format!(
                "Your context is at {}% and you just requested a review of PR #{pr_number}, a good point to hand off.",
                round_percent(*percent)
            ),
            Self::ReviewRequest {
                percent: None,
                pr_number,
            } => format!(
                "You just requested a review of PR #{pr_number}, a good point to hand off."
            ),
            Self::DocReview {
                percent,
                owner_name,
                doc_title,
            } => {
                let opening = match percent {
                    Some(percent) => format!(
                        "Your context is at {}% and you just asked",
                        round_percent(*percent)
                    ),
                    None => "You just asked".to_owned(),
                };
                format!(
                    "{opening} {owner_name} to review {doc_title}, a good point to hand off. His review will wake your successor."
                )
            }
            Self::Owner { owner_name } => format!("{owner_name} asked you to hand off."),
        }
    }
}

/// D.1. `claims` is already rendered by [`claims_text`].
pub fn ask_text(reason: &AskReason<'_>, claims: &str) -> String {
    format!(
        "[sm context management] {} Stop at a logical point. sm will move your claims ({claims}) and pending wakes to a fresh agent. Post what the next agent needs in your PR, ticket, or a file, then run `sm handoff --link <url>` or `sm handoff --path <file>` and end your turn.",
        reason.text()
    )
}

/// D.2.
pub fn reminder_text(percent: f64, asked_percent: f64) -> String {
    format!(
        "[sm context management] Reminder: your context is at {}% and sm asked you to hand off at {}%. Finish the current step, post your handoff note, and run `sm handoff --link <url>` or `sm handoff --path <file>`.",
        round_percent(percent),
        round_percent(asked_percent)
    )
}

/// `ticket #1651, PR #1660` in claim order, or `none`. Each item is
/// (kind noun, number).
pub fn claims_text(claims: &[(&str, i64)]) -> String {
    if claims.is_empty() {
        return "none".to_owned();
    }
    claims
        .iter()
        .map(|(noun, number)| format!("{noun} #{number}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn round_percent(value: f64) -> i64 {
    value.round() as i64
}

/// A session as the handoff view names it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionRef {
    pub id: String,
    pub name: String,
}

/// The `display` string of Appendix I.2. The server computes it so the three
/// surfaces never diverge.
pub fn display(
    policy: &EffectivePolicy,
    record: Option<&HandoffRecord>,
    successor: Option<&SessionRef>,
) -> String {
    let Some(record) = record else {
        return if policy.enabled {
            format!("hands off at {}%", format_percent(policy.threshold_percent))
        } else {
            "handoff off".to_owned()
        };
    };
    match record.state {
        HandoffPhase::Asked if record.reminded_at.is_some() => "handoff overdue".to_owned(),
        HandoffPhase::Asked => match record.asked_at.as_deref().and_then(local_hh_mm) {
            Some(at) => format!("asked {at}"),
            None => "asked".to_owned(),
        },
        HandoffPhase::Accepted | HandoffPhase::Spawning | HandoffPhase::Transferring => {
            "handing off".to_owned()
        }
        HandoffPhase::Failed => "handoff failed".to_owned(),
        HandoffPhase::Done => match (successor, record.successor_session_id.as_deref()) {
            (Some(successor), _) => format!("→ {}", successor.name),
            (None, Some(id)) => format!("→ {id}"),
            (None, None) => "handed off".to_owned(),
        },
    }
}

/// The `handoff` object of session JSON (Appendix I.2).
pub fn view_json(
    policy: &EffectivePolicy,
    record: Option<&HandoffRecord>,
    successor: Option<&SessionRef>,
    predecessor: Option<&SessionRef>,
) -> Value {
    json!({
        "enabled": policy.enabled,
        "threshold_percent": percent_json(policy.threshold_percent),
        "source": policy.source,
        "has_gauge": policy.has_gauge,
        "display": display(policy, record, successor),
        "state": record.map(|record| record.state.as_str()),
        "successor": successor,
        "predecessor": predecessor,
    })
}

fn local_hh_mm(timestamp: &str) -> Option<String> {
    let at = OffsetDateTime::parse(timestamp.trim(), &Rfc3339).ok()?;
    let local = crate::queue::local_now_naive(at)?;
    Some(format!("{:02}:{:02}", local.hour(), local.minute()))
}

fn valid_percent(value: f64, allow_zero: bool) -> bool {
    value.is_finite()
        && value <= 100.0
        && if allow_zero {
            value >= 0.0
        } else {
            value > 0.0
        }
}

fn percent_field(field: &str, value: &Value, allow_zero: bool) -> Result<f64, String> {
    let range = if allow_zero { "[0, 100]" } else { "(0, 100]" };
    value
        .as_f64()
        .filter(|value| valid_percent(*value, allow_zero))
        .ok_or_else(|| format!("{field} must be a number in {range}"))
}

fn bool_field(field: &str, value: &Value) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("{field} must be true or false"))
}

/// Whole percents serialize as integers: `35`, not `35.0`.
fn percent_json(value: f64) -> Value {
    if value.fract() == 0.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn format_percent(value: f64) -> String {
    percent_json(value).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn over(enabled: Option<bool>, threshold: Option<f64>) -> HandoffOverride {
        HandoffOverride {
            enabled,
            threshold_percent: threshold,
            set_at: None,
        }
    }

    #[test]
    fn resolution_table_matches_appendix_b() {
        let defaults = HandoffDefaults::default();
        let claude = effective_policy(&defaults, "claude", None, true);
        assert!(claude.enabled);
        assert_eq!(claude.threshold_percent, 35.0);
        assert_eq!(claude.reminder_at(35.0), Some(50.0));
        assert_eq!(claude.source, "default");

        let o = over(None, Some(45.0));
        let claude45 = effective_policy(&defaults, "claude", Some(&o), true);
        assert!(claude45.enabled);
        assert_eq!(claude45.threshold_percent, 45.0);
        assert_eq!(claude45.reminder_at(45.0), Some(50.0));
        assert_eq!(claude45.source, "override");

        let o = over(Some(false), None);
        assert!(!effective_policy(&defaults, "claude", Some(&o), true).enabled);

        assert!(!effective_policy(&defaults, "codex-fork", None, true).enabled);

        let o = over(Some(true), None);
        let fork_on = effective_policy(&defaults, "codex-fork", Some(&o), true);
        assert!(fork_on.enabled);
        assert_eq!(fork_on.threshold_percent, 80.0);
        assert_eq!(fork_on.reminder_at(80.0), Some(90.0));

        let app_on = effective_policy(&defaults, "codex-app", Some(&o), false);
        assert!(app_on.enabled);
        assert!(!app_on.has_gauge);

        assert!(!effective_policy(&defaults, "some-new-provider", None, false).enabled);
    }

    #[test]
    fn reminder_ten_points_after_an_ask_at_or_past_the_stored_reminder() {
        let defaults = HandoffDefaults::default();
        let claude = effective_policy(&defaults, "claude", None, true);
        assert_eq!(claude.reminder_at(26.0), Some(50.0));
        assert_eq!(claude.reminder_at(50.0), Some(60.0));
        assert_eq!(claude.reminder_at(55.0), Some(65.0));
        assert_eq!(claude.reminder_at(85.0), Some(90.0));
        assert_eq!(claude.reminder_at(90.0), None);
        assert_eq!(claude.reminder_at(97.0), None);
        let o = over(Some(true), None);
        let fork = effective_policy(&defaults, "codex-fork", Some(&o), true);
        assert_eq!(fork.reminder_at(80.0), Some(90.0));
        assert_eq!(fork.reminder_at(92.0), None);
    }

    #[test]
    fn defaults_merge_and_validate() {
        let defaults = HandoffDefaults::default();
        let merged = defaults
            .merged(&json!({"threshold_percent": 40, "providers": {"codex-fork": true}}))
            .unwrap();
        assert_eq!(merged.threshold_percent, 40.0);
        assert!(
            merged.provider_enabled("claude"),
            "untouched provider keeps its value"
        );
        assert!(merged.provider_enabled("codex-fork"));
        assert_eq!(merged.review_floor_percent, 20.0);

        assert_eq!(
            defaults
                .merged(&json!({"threshold_percent": 0}))
                .unwrap_err(),
            "threshold_percent must be a number in (0, 100]"
        );
        assert_eq!(
            defaults
                .merged(&json!({"reminder_percent": 101}))
                .unwrap_err(),
            "reminder_percent must be a number in (0, 100]"
        );
        assert!(defaults.merged(&json!({"review_floor_percent": 0})).is_ok());
        assert_eq!(
            defaults
                .merged(&json!({"review_floor_percent": -1}))
                .unwrap_err(),
            "review_floor_percent must be a number in [0, 100]"
        );
        assert_eq!(
            defaults
                .merged(&json!({"ask_on_doc_review": "yes"}))
                .unwrap_err(),
            "ask_on_doc_review must be true or false"
        );
        assert_eq!(
            defaults.merged(&json!({"bogus": 1})).unwrap_err(),
            "unknown field bogus"
        );
    }

    #[test]
    fn stored_defaults_fill_missing_and_invalid_fields() {
        let stored = json!({"threshold_percent": 500, "reminder_percent": 60, "updated_at": "t"});
        let defaults = HandoffDefaults::from_stored(Some(&stored));
        assert_eq!(defaults.threshold_percent, 35.0);
        assert_eq!(defaults.reminder_percent, 60.0);
        assert_eq!(defaults.updated_at.as_deref(), Some("t"));
        assert_eq!(
            HandoffDefaults::default().to_json()["threshold_percent"],
            json!(35)
        );
    }

    #[test]
    fn legacy_flat_thresholds_migrate_only_to_claude_and_flat_put_stays_compatible() {
        let old =
            json!({"threshold_percent": 42, "reminder_percent": 55, "review_floor_percent": 12});
        let defaults = HandoffDefaults::from_stored(Some(&old));
        assert_eq!(defaults.thresholds("claude").threshold_percent, 42.0);
        assert_eq!(defaults.thresholds("codex-fork").threshold_percent, 80.0);
        let updated = defaults.merged(&json!({"threshold_percent": 37})).unwrap();
        assert_eq!(
            updated.to_json()["provider_thresholds"]["claude"]["threshold_percent"],
            37
        );
        assert_eq!(
            updated.to_json()["provider_thresholds"]["codex-fork"]["threshold_percent"],
            80
        );
        let provider_update = updated
            .merged(&json!({"provider_thresholds": {"codex-fork": {"threshold_percent": 75}}}))
            .unwrap();
        assert_eq!(provider_update.threshold_percent, 37.0);
        assert_eq!(
            provider_update.thresholds("codex-fork").threshold_percent,
            75.0
        );
        assert_eq!(
            HandoffDefaults::from_stored(Some(&provider_update.to_json())),
            provider_update
        );
        let full_body = json!({
            "threshold_percent": 37,
            "provider_thresholds": {"claude": {"threshold_percent": 31}}
        });
        let explicit = provider_update.merged(&full_body).unwrap();
        assert_eq!(explicit.threshold_percent, 31.0);
        assert_eq!(explicit.thresholds("claude").threshold_percent, 31.0);
    }

    #[test]
    fn ticket_fields_fill_only_unset_agent_fields() {
        let defaults = HandoffDefaults::default();
        let ticket = over(Some(true), Some(30.0));
        let agent = over(None, Some(45.0));
        let policy = effective_policy_with_ticket(
            &defaults,
            "codex-fork",
            Some(&agent),
            Some(&ticket),
            true,
        );
        assert!(policy.enabled);
        assert_eq!(policy.threshold_percent, 45.0);
        assert_eq!(policy.source, "override");
        let inherited =
            effective_policy_with_ticket(&defaults, "codex-fork", None, Some(&ticket), true);
        assert_eq!(inherited.threshold_percent, 30.0);
        assert_eq!(inherited.source, "ticket");
    }

    #[test]
    fn policy_update_parse_and_apply() {
        let update = PolicyUpdate::parse(&json!({"threshold_percent": 45})).unwrap();
        let next = update.apply(None, "now").unwrap();
        assert_eq!(next.threshold_percent, Some(45.0));
        assert_eq!(next.enabled, None);

        let update = PolicyUpdate::parse(&json!({"threshold_percent": null})).unwrap();
        assert_eq!(update.apply(Some(&next), "now"), None);

        let update = PolicyUpdate::parse(&json!({"use_default": true})).unwrap();
        assert_eq!(update.apply(Some(&next), "now"), None);

        assert!(PolicyUpdate::parse(&json!({"use_default": true, "enabled": false})).is_err());
        assert!(PolicyUpdate::parse(&json!({"threshold_percent": 0})).is_err());
        assert!(PolicyUpdate::parse(&json!({"requester_session_id": "x"})).is_err());
        let ask = PolicyUpdate::parse(&json!({"ask_now": true})).unwrap();
        assert!(ask.ask_now && !ask.changes_override());
    }

    #[test]
    fn ask_texts_are_verbatim() {
        let claims = claims_text(&[("ticket", 1651), ("PR", 1660)]);
        assert_eq!(claims, "ticket #1651, PR #1660");
        assert_eq!(claims_text(&[]), "none");
        let tail = " Stop at a logical point. sm will move your claims (ticket #1651, PR #1660) and pending wakes to a fresh agent. Post what the next agent needs in your PR, ticket, or a file, then run `sm handoff --link <url>` or `sm handoff --path <file>` and end your turn.";
        assert_eq!(
            ask_text(&AskReason::Context { percent: 41.4 }, &claims),
            format!("[sm context management] Your context is at 41%.{tail}")
        );
        assert_eq!(
            ask_text(
                &AskReason::ReviewRequest {
                    percent: Some(24.0),
                    pr_number: 1660
                },
                &claims
            ),
            format!("[sm context management] Your context is at 24% and you just requested a review of PR #1660, a good point to hand off.{tail}")
        );
        assert_eq!(
            ask_text(
                &AskReason::ReviewRequest {
                    percent: None,
                    pr_number: 1660
                },
                &claims
            ),
            format!("[sm context management] You just requested a review of PR #1660, a good point to hand off.{tail}")
        );
        assert_eq!(
            ask_text(
                &AskReason::DocReview {
                    percent: Some(22.0),
                    owner_name: "Rajesh",
                    doc_title: "Context handoff"
                },
                &claims
            ),
            format!("[sm context management] Your context is at 22% and you just asked Rajesh to review Context handoff, a good point to hand off. His review will wake your successor.{tail}")
        );
        assert_eq!(
            ask_text(
                &AskReason::DocReview {
                    percent: None,
                    owner_name: "Rajesh",
                    doc_title: "Context handoff"
                },
                &claims
            ),
            format!("[sm context management] You just asked Rajesh to review Context handoff, a good point to hand off. His review will wake your successor.{tail}")
        );
        assert_eq!(
            ask_text(
                &AskReason::Owner {
                    owner_name: "Rajesh"
                },
                &claims
            ),
            format!("[sm context management] Rajesh asked you to hand off.{tail}")
        );
        assert_eq!(
            reminder_text(50.2, 36.4),
            "[sm context management] Reminder: your context is at 50% and sm asked you to hand off at 36%. Finish the current step, post your handoff note, and run `sm handoff --link <url>` or `sm handoff --path <file>`."
        );
    }

    #[test]
    fn display_strings_per_state() {
        let defaults = HandoffDefaults::default();
        let on = effective_policy(&defaults, "claude", None, true);
        let off = effective_policy(&defaults, "codex-fork", None, true);
        assert_eq!(display(&on, None, None), "hands off at 35%");
        assert_eq!(display(&off, None, None), "handoff off");

        let mut record =
            HandoffRecord::asked(HandoffTrigger::Context, "2026-09-29T14:02:11Z", Some(36.0));
        let asked = display(&on, Some(&record), None);
        assert!(
            asked.starts_with("asked ") && asked.len() == "asked 14:02".len(),
            "{asked}"
        );
        record.reminded_at = Some("2026-09-29T15:00:00Z".to_owned());
        assert_eq!(display(&on, Some(&record), None), "handoff overdue");
        for phase in [
            HandoffPhase::Accepted,
            HandoffPhase::Spawning,
            HandoffPhase::Transferring,
        ] {
            record.state = phase;
            assert_eq!(display(&on, Some(&record), None), "handing off");
        }
        record.state = HandoffPhase::Failed;
        assert_eq!(display(&on, Some(&record), None), "handoff failed");
        record.state = HandoffPhase::Done;
        let successor = SessionRef {
            id: "7c1e".to_owned(),
            name: "sm-1651-engineer-h2".to_owned(),
        };
        assert_eq!(
            display(&on, Some(&record), Some(&successor)),
            "→ sm-1651-engineer-h2"
        );

        let view = view_json(&on, None, None, None);
        assert_eq!(view["threshold_percent"], json!(35));
        assert_eq!(view["source"], "default");
        assert_eq!(view["state"], Value::Null);
    }
}
