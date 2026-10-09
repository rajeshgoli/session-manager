//! Model lifecycle owned by sm (#1957). No process here runs as a queue job.
//! #1958 adds delivery holds, parked activity and automatic reload around the
//! durable loading/ready/draining/yielded states exposed by this module.
use crate::{
    config::AppConfig,
    sessions::{expand_home, SessionStore},
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

const GB: i64 = 1_000_000_000;
pub const FLASH_KEY: &str = "Youssofal/Qwen3.8-Flash-Next-MTPLX-Optimized-Speed";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LocalHostConfig {
    pub server: String,
    pub base_url: String,
    pub auth_token: String,
    pub mtplx_path: String,
    pub lms_path: String,
    pub model_root: String,
    pub lmstudio_server_config: String,
    pub lmstudio_settings: String,
    pub lmstudio_app_info: String,
    pub context_window: u64,
    pub session_bank_max_bytes: i64,
    pub yield_margin_bytes: i64,
    pub drain_timeout: u64,
    pub reload_hold: u64,
}
impl Default for LocalHostConfig {
    fn default() -> Self {
        Self {
            server: "mtplx".into(),
            base_url: "http://127.0.0.1:8000".into(),
            auth_token: "local".into(),
            mtplx_path: "~/.local/share/mtplx-venv/bin/mtplx".into(),
            lms_path: "~/.lmstudio/bin/lms".into(),
            model_root: "~/.lmstudio/models".into(),
            lmstudio_server_config: "~/.lmstudio/.internal/http-server-config.json".into(),
            lmstudio_settings: "~/.lmstudio/settings.json".into(),
            lmstudio_app_info: "/Applications/LM Studio.app/Contents/Info.plist".into(),
            context_window: 200_000,
            session_bank_max_bytes: 16 * GB,
            yield_margin_bytes: 24 * GB,
            drain_timeout: 600,
            reload_hold: 600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRecord {
    pub key: String,
    pub server: String,
    pub identifier: String,
    pub seats: u32,
    pub context: u64,
    pub reservation_bytes: i64,
    pub measured_peak_bytes: i64,
    pub state: String,
    pub desired: bool,
    pub state_since: String,
    pub last_yield_reason: Option<String>,
    pub last_yield_at: Option<String>,
    pub last_error: Option<String>,
    pub pid: Option<i32>,
    pub endpoint: String,
}
impl ModelRecord {
    pub fn resident(&self) -> bool {
        matches!(self.state.as_str(), "loading" | "ready" | "draining")
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadRequest {
    pub key: String,
    pub seats: Option<u32>,
    pub context: Option<u64>,
    /// Decimal GB, matching the model memo and CLI.
    pub reservation: Option<f64>,
}

/// Both backends obey the same lifecycle. Stop must confirm memory has been
/// released and must never force-kill the model process.
trait ModelServer: Send + Sync {
    fn preflight(&self, _model: &ModelRecord) -> Result<()> {
        Ok(())
    }
    fn start(&self, model: &ModelRecord) -> Result<Option<i32>>;
    fn ready(&self, model: &ModelRecord) -> Result<bool>;
    fn stop(&self, model: &ModelRecord) -> Result<()>;
    fn running(&self, model: &ModelRecord) -> Result<bool> {
        self.ready(model)
    }
    /// PID recovered only from the backend's private ownership boundary.
    fn owned_pid(&self, model: &ModelRecord) -> Result<Option<i32>> {
        Ok(model.pid)
    }
    fn footprint(&self, model: &ModelRecord) -> Result<Option<i64>>;
}

pub struct ModelHost {
    config: LocalHostConfig,
    db_path: PathBuf,
    state_file: PathBuf,
    queue_dir: PathBuf,
    queue_policy: crate::queue::QueueAdmissionPolicy,
    operation: Mutex<()>,
    yield_worker: AtomicBool,
    force_unload: AtomicBool,
    backend: Arc<dyn ModelServer>,
}
impl ModelHost {
    fn connect(&self) -> Result<Connection> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS local_model (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), key TEXT NOT NULL,
            server TEXT NOT NULL, identifier TEXT NOT NULL, seats INTEGER NOT NULL,
            context INTEGER NOT NULL, reservation_bytes INTEGER NOT NULL,
            measured_peak_bytes INTEGER NOT NULL, state TEXT NOT NULL, desired INTEGER NOT NULL,
            state_since TEXT NOT NULL, last_yield_reason TEXT, last_yield_at TEXT,
            last_error TEXT, pid INTEGER, endpoint TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS local_model_peaks (
            key TEXT NOT NULL, server TEXT NOT NULL, seats INTEGER NOT NULL,
            context INTEGER NOT NULL, measured_peak_bytes INTEGER NOT NULL,
            PRIMARY KEY(key,server,seats,context));",
        )?;
        Ok(conn)
    }
    pub fn record(&self) -> Result<Option<ModelRecord>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT key,server,identifier,seats,context,
            reservation_bytes,measured_peak_bytes,state,desired,state_since,
            last_yield_reason,last_yield_at,last_error,pid,endpoint FROM local_model WHERE singleton=1",
                [],
                |r| {
                    Ok(ModelRecord {
                        key: r.get(0)?,
                        server: r.get(1)?,
                        identifier: r.get(2)?,
                        seats: r.get(3)?,
                        context: r.get(4)?,
                        reservation_bytes: r.get(5)?,
                        measured_peak_bytes: r.get(6)?,
                        state: r.get(7)?,
                        desired: r.get(8)?,
                        state_since: r.get(9)?,
                        last_yield_reason: r.get(10)?,
                        last_yield_at: r.get(11)?,
                        last_error: r.get(12)?,
                        pid: r.get(13)?,
                        endpoint: r.get(14)?,
                    })
                },
            )
            .optional()?)
    }
    fn save(&self, m: &ModelRecord) -> Result<()> {
        self.connect()?.execute(
            "INSERT INTO local_model VALUES
            (1,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
            ON CONFLICT(singleton) DO UPDATE SET
            key=excluded.key,server=excluded.server,identifier=excluded.identifier,
            seats=excluded.seats,context=excluded.context,
            reservation_bytes=CASE WHEN local_model.key=excluded.key AND local_model.server=excluded.server AND local_model.seats=excluded.seats AND local_model.context=excluded.context
              THEN MAX(local_model.reservation_bytes,excluded.reservation_bytes) ELSE excluded.reservation_bytes END,
            measured_peak_bytes=CASE WHEN local_model.key=excluded.key AND local_model.server=excluded.server AND local_model.seats=excluded.seats AND local_model.context=excluded.context
              THEN MAX(local_model.measured_peak_bytes,excluded.measured_peak_bytes) ELSE excluded.measured_peak_bytes END,
            state=excluded.state,desired=excluded.desired,state_since=excluded.state_since,
            last_yield_reason=excluded.last_yield_reason,last_yield_at=excluded.last_yield_at,
            last_error=excluded.last_error,pid=excluded.pid,endpoint=excluded.endpoint",
            params![
                m.key,
                m.server,
                m.identifier,
                m.seats,
                m.context,
                m.reservation_bytes,
                m.measured_peak_bytes,
                m.state,
                m.desired,
                m.state_since,
                m.last_yield_reason,
                m.last_yield_at,
                m.last_error,
                m.pid,
                m.endpoint
            ],
        )?;
        Ok(())
    }
    fn transition(&self, m: &mut ModelRecord, state: &str) -> Result<()> {
        m.state = state.into();
        m.state_since = now();
        self.save(m)
    }
    pub fn status(&self) -> Result<Value> {
        let model = self.record()?;
        let used = self.local_sessions()?.len();
        Ok(serde_json::json!({"model":model,"seats_used":used,"judge_seats":usize::from(used>0)}))
    }
    fn local_sessions(&self) -> Result<Vec<crate::sessions::SessionRecord>> {
        if !self.state_file.exists() {
            return Ok(vec![]);
        }
        Ok(SessionStore::new(self.state_file.clone())
            .list_sessions(false)?
            .into_iter()
            .filter(|s| {
                // #1956 owns the host field. Serializing keeps this component
                // independent of the chosen harness and sees it once introduced.
                serde_json::to_value(s)
                    .ok()
                    .is_some_and(|v| v["host"] == "local")
            })
            .collect())
    }
    pub fn load(&self, request: LoadRequest) -> Result<ModelRecord> {
        let _lock = self
            .operation
            .lock()
            .map_err(|_| anyhow::anyhow!("model operation lock poisoned"))?;
        if self.record()?.is_some_and(|m| m.resident()) {
            bail!("a local model is already loaded or changing state; unload it first");
        }
        let mut model = self.prepare(request)?;
        let available = crate::queue::host_memory_capacity()
            .context("host memory unavailable; model load refused")?
            .1;
        self.load_prepared(&mut model, available)?;
        Ok(model)
    }
    fn prepare(&self, request: LoadRequest) -> Result<ModelRecord> {
        let seats = request.seats.unwrap_or(1);
        let context = request.context.unwrap_or(self.config.context_window);
        if request.key.trim().is_empty() || request.key.starts_with('-') {
            bail!("model key must be nonempty and cannot start with '-' ");
        }
        if seats == 0 || seats > 64 || !(100_000..=2_000_000).contains(&context) {
            bail!("seats must be 1..64 and context must be 100000..2000000");
        }
        let previous: i64 = self
            .connect()?
            .query_row(
                "SELECT measured_peak_bytes FROM local_model_peaks
            WHERE key=?1 AND server=?2 AND seats=?3 AND context=?4",
                params![request.key, self.config.server, seats, context],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let seeded = if self.config.server == "mtplx" && request.key == FLASH_KEY {
            match (seats, context) {
                (1, 200_000) => 137 * GB,
                (1, 100_000..=199_999) => 139 * GB,
                (2, 100_000..=160_000) => 164 * GB,
                (2, 160_001..=200_000) => 176 * GB,
                _ => 0,
            }
        } else {
            0
        };
        let peak = previous.max(seeded);
        let explicit = request.reservation.map(gb_bytes).transpose()?;
        let reservation = if self.config.server == "mtplx" {
            explicit.unwrap_or(0).max(peak_margin(peak)?)
        } else {
            let path = expand_home(&self.config.model_root).join(&request.key);
            let formula = lmstudio_reservation(&path, seats, context);
            match (formula, explicit) {
                (Ok(floor), e) => floor.max(peak).max(e.unwrap_or(0)),
                (Err(_), Some(e)) => e.max(peak),
                (Err(error), None) => return Err(error),
            }
        };
        if reservation == 0 {
            bail!("no measured peak for this model and seat/context configuration; supply --reservation <GB>");
        }
        Ok(ModelRecord {
            identifier: identifier(&request.key),
            key: request.key,
            server: self.config.server.clone(),
            seats,
            context,
            reservation_bytes: reservation,
            measured_peak_bytes: peak,
            state: "unloaded".into(),
            desired: true,
            state_since: now(),
            last_yield_reason: None,
            last_yield_at: None,
            last_error: None,
            pid: None,
            endpoint: self.config.base_url.clone(),
        })
    }
    fn load_prepared(&self, m: &mut ModelRecord, available: i64) -> Result<()> {
        let required = m
            .reservation_bytes
            .checked_add(self.yield_line())
            .context("reservation overflow")?;
        if available < required {
            bail!(
                "model needs {} GB including yield headroom; available {} GB",
                required / GB,
                available / GB
            );
        }
        self.backend.preflight(m)?;
        crate::queue::with_model_load_admission(&self.queue_dir, self.queue_policy, || {
            self.transition(m, "loading")
        })?;
        let result = (|| {
            m.pid = self.backend.start(m)?;
            self.save(m)?;
            let deadline = Instant::now() + Duration::from_secs(600);
            loop {
                if crate::queue::host_memory_capacity().is_none_or(|(_, a)| a < self.yield_line()) {
                    bail!("host memory fell below the yield line during model loading");
                }
                if self.backend.ready(m)? {
                    return self.transition(m, "ready");
                }
                if Instant::now() >= deadline {
                    bail!("model readiness timed out after 10 minutes");
                }
                thread::sleep(Duration::from_secs(1));
            }
        })();
        if let Err(error) = result {
            m.last_error = Some(format!("{error:#}"));
            m.desired = false;
            self.transition(m, "draining")?;
            match self.backend.stop(m) {
                Ok(()) => {
                    m.pid = None;
                    self.transition(m, "unloaded")?;
                }
                Err(stop) => {
                    m.last_error = Some(format!("{error:#}; unload failed: {stop:#}"));
                    self.save(m)?;
                }
            }
            return Err(error);
        }
        Ok(())
    }
    fn yield_line(&self) -> i64 {
        // Read each time: the reserve rises with kernel pressure (sm#2053).
        crate::queue::effective_memory_reserve_bytes(self.queue_policy.memory_min_free_bytes)
            .saturating_add(self.config.yield_margin_bytes.max(0))
    }
    pub fn unload(&self, force: bool, reason: Option<&str>) -> Result<()> {
        let _lock = self
            .operation
            .lock()
            .map_err(|_| anyhow::anyhow!("model operation lock poisoned"))?;
        if force {
            self.force_unload.store(true, Ordering::SeqCst);
        }
        let result = self.unload_locked(force, reason);
        if result.is_ok() {
            self.force_unload.store(false, Ordering::SeqCst);
        }
        result
    }
    fn unload_locked(&self, force: bool, reason: Option<&str>) -> Result<()> {
        let Some(mut m) = self.record()? else {
            return Ok(());
        };
        if reason.is_none() {
            m.desired = false;
            self.save(&m)?;
        }
        if !m.resident() {
            if !m.desired {
                self.transition(&mut m, "unloaded")?;
            }
            return Ok(());
        }
        if let Some(reason) = reason {
            m.last_yield_reason = Some(reason.into());
            m.last_yield_at = Some(now());
        }
        self.transition(&mut m, "draining")?;
        if !force {
            let deadline = Instant::now() + Duration::from_secs(self.config.drain_timeout);
            while self.local_sessions()?.iter().any(|s| {
                crate::sessions::claude_hook_gate(s) != crate::sessions::ClaudeHookGate::TurnStopped
            }) {
                if Instant::now() >= deadline || self.force_unload.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(Duration::from_secs(1));
            }
        }
        if let Err(error) = self.backend.stop(&m) {
            m.last_error = Some(format!("unload failed: {error:#}"));
            self.save(&m)?;
            return Err(error);
        }
        m.pid = None;
        m.last_error = None;
        let state = if m.desired { "yielded" } else { "unloaded" };
        self.transition(&mut m, state)
    }
    /// Nonblocking admission hook: queue admission never waits on shutdown.
    fn request_yield(self: &Arc<Self>, reason: String, force: bool) -> Result<bool> {
        let resident = self.record()?.is_some_and(|m| m.resident());
        if !resident {
            return Ok(false);
        }
        if force {
            self.force_unload.store(true, Ordering::SeqCst);
        }
        if self
            .yield_worker
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let host = self.clone();
            thread::spawn(move || {
                if let Err(error) = host.unload(force, Some(&reason)) {
                    eprintln!("local model yield: {error:#}");
                }
                host.yield_worker.store(false, Ordering::SeqCst);
            });
        }
        Ok(true)
    }
    fn recover(&self) -> Result<()> {
        if let Some(mut m) = self.record()?.filter(ModelRecord::resident) {
            if !self.backend.running(&m)? {
                m.pid = None;
                let state = if m.desired { "yielded" } else { "unloaded" };
                self.transition(&mut m, state)?;
            } else if m.state == "loading" {
                if m.pid.is_none() {
                    // Startup may have persisted loading before the tmux launch
                    // returned its PID. Recover only from our private pane.
                    m.pid = self.backend.owned_pid(&m)?;
                }
                // A server restart interrupted a load worker. Keep admission shut
                // until a deliberate unload cleans up this still-resident model.
                m.last_error = Some("sm restarted during loading; unload before reloading".into());
                self.transition(&mut m, "draining")?;
            }
        }
        Ok(())
    }
    /// The runtime repairs a timed-out shutdown only after the backend confirms
    /// absence. Never race a load/unload worker or interrupt an active drain.
    pub fn reconcile_draining(&self) -> Result<()> {
        let _lock = match self.operation.try_lock() {
            Ok(lock) => lock,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                bail!("model operation lock poisoned");
            }
        };
        if let Some(mut m) = self.record()?.filter(|m| m.state == "draining") {
            if !self.backend.running(&m)? {
                m.pid = None;
                m.last_error = None;
                let state = if m.desired { "yielded" } else { "unloaded" };
                self.transition(&mut m, state)?;
                self.force_unload.store(false, Ordering::SeqCst);
            }
        }
        Ok(())
    }
    fn sample(&self) -> Result<Option<i64>> {
        let Some(m) = self.record()?.filter(ModelRecord::resident) else {
            return Ok(Some(0));
        };
        let footprint = self.backend.footprint(&m)?;
        if let Some(bytes) = footprint.filter(|b| *b > m.measured_peak_bytes) {
            let reservation = if m.server == "mtplx" {
                peak_margin(bytes)?
            } else {
                bytes
            };
            // Only update measurements: concurrent lifecycle transitions win.
            let conn = self.connect()?;
            conn.execute("UPDATE local_model SET measured_peak_bytes=MAX(measured_peak_bytes,?1),
                reservation_bytes=MAX(reservation_bytes,?2) WHERE singleton=1 AND key=?3 AND server=?4 AND seats=?5 AND context=?6",
                params![bytes,reservation,m.key,m.server,m.seats,m.context])?;
            conn.execute("INSERT INTO local_model_peaks VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(key,server,seats,context) DO UPDATE SET measured_peak_bytes=MAX(measured_peak_bytes,excluded.measured_peak_bytes)",
                params![m.key,m.server,m.seats,m.context,bytes])?;
        }
        Ok(footprint)
    }
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC time formats")
}
fn gb_bytes(gb: f64) -> Result<i64> {
    if !gb.is_finite() || gb <= 0. || gb >= i64::MAX as f64 / GB as f64 {
        bail!("reservation must be a positive finite number of GB");
    }
    Ok((gb * GB as f64).ceil() as i64)
}
fn peak_margin(peak: i64) -> Result<i64> {
    peak.checked_mul(11)
        .and_then(|p| p.checked_add(9))
        .map(|p| p / 10)
        .context("measured peak overflow")
}
fn identifier(key: &str) -> String {
    let name = key.rsplit('/').next().unwrap_or(key).to_lowercase();
    name.split("-mtplx-")
        .next()
        .unwrap_or(&name)
        .split("-mlx-")
        .next()
        .unwrap_or(&name)
        .into()
}
fn lmstudio_reservation(path: &Path, seats: u32, context: u64) -> Result<i64> {
    let config: Value = serde_json::from_slice(
        &fs::read(path.join("config.json"))
            .context("unrecognised attention layout; supply --reservation <GB>")?,
    )?;
    let config = config.get("text_config").unwrap_or(&config);
    let number = |key: &str| {
        config[key]
            .as_u64()
            .context("unrecognised attention layout; supply --reservation <GB>")
    };
    let layers = number("num_hidden_layers")?;
    let full = if let Some(layout) = config["layer_types"].as_array() {
        layout
            .iter()
            .filter(|v| v.as_str() == Some("full_attention"))
            .count() as u64
    } else if let Some(interval) = config["full_attention_interval"]
        .as_u64()
        .filter(|i| *i > 0)
    {
        layers / interval
    } else if config["sliding_window"].as_u64().is_some() {
        bail!("unrecognised attention layout; supply --reservation <GB>");
    } else {
        layers
    };
    if full == 0 {
        bail!("unrecognised attention layout; supply --reservation <GB>");
    }
    let heads = number("num_key_value_heads")?;
    let dim = config["head_dim"]
        .as_u64()
        .or_else(|| {
            config["hidden_size"]
                .as_u64()?
                .checked_div(config["num_attention_heads"].as_u64()?)
        })
        .context("unrecognised attention layout; supply --reservation <GB>")?;
    let mut weights = 0u64;
    for file in fs::read_dir(path)? {
        let file = file?;
        if file.path().extension().is_some_and(|e| e == "safetensors") {
            weights = weights
                .checked_add(file.metadata()?.len())
                .context("weights overflow")?;
        }
    }
    if weights == 0 {
        bail!("no model weights on disk; supply --reservation <GB>");
    }
    let mut cache = 4u64;
    for n in [full, heads, dim, u64::from(seats), context] {
        cache = cache.checked_mul(n).context("cache reservation overflow")?;
    }
    Ok(i64::try_from(
        weights.checked_add(cache).context("reservation overflow")?,
    )?)
}

struct CliServer {
    config: LocalHostConfig,
    socket: String,
    port: u16,
}
impl CliServer {
    fn tmux(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("tmux");
        cmd.args(["-L", &self.socket]).args(args);
        command_output(cmd, Duration::from_secs(5))
    }
    fn pane(&self) -> Result<Option<i32>> {
        // A missing tmux session is absent, not proof that an HTTP server at
        // the same port belongs to sm. Ownership is always this private pane.
        let text = self.tmux(&[
            "display-message",
            "-p",
            "-t",
            "model",
            "#{pane_dead} #{pane_pid}",
        ]);
        match text {
            Ok(text) => live_pane_pid(&text),
            Err(error) => {
                let detail = format!("{error:#}");
                if detail.contains("can't find")
                    || detail.contains("no server running")
                    || detail.contains("error connecting")
                {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    }
    fn models(&self) -> Result<Value> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2)))
            .build()
            .into();
        let mut response = agent
            .get(&format!(
                "{}/v1/models",
                self.config.base_url.trim_end_matches('/')
            ))
            .header(
                "Authorization",
                &format!("Bearer {}", self.config.auth_token),
            )
            .call()?;
        Ok(serde_json::from_str(
            &response.body_mut().read_to_string()?,
        )?)
    }
    fn lms_models(&self) -> Result<Value> {
        let mut c = Command::new(expand_home(&self.config.lms_path));
        c.args(["ps", "--json"]);
        Ok(serde_json::from_str(&command_output(
            c,
            Duration::from_secs(5),
        )?)?)
    }
    fn lms_has(&self, id: &str) -> Result<bool> {
        let models = self.lms_models()?;
        let models = models
            .as_array()
            .context("lms ps --json did not return an array")?;
        Ok(models.iter().any(|m| m["identifier"].as_str() == Some(id)))
    }
}
impl ModelServer for CliServer {
    fn preflight(&self, m: &ModelRecord) -> Result<()> {
        if m.server == "lmstudio" {
            let mut version = Command::new("/usr/bin/plutil");
            version
                .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
                .arg(expand_home(&self.config.lmstudio_app_info));
            if command_output(version, Duration::from_secs(5))?.trim() != "0.4.16" {
                bail!("sm requires LM Studio 0.4.16; auto-update must stay off");
            }
            let app: Value =
                serde_json::from_slice(&fs::read(expand_home(&self.config.lmstudio_settings))?)?;
            if app["developer"]["autoUpdateExtensionPacks"] != false {
                bail!("disable LM Studio automatic engine updates before sm model load");
            }
            let settings: Value = serde_json::from_slice(&fs::read(expand_home(
                &self.config.lmstudio_server_config,
            ))?)?;
            if settings["justInTimeModelLoading"] != false {
                bail!("disable LM Studio just-in-time loading before sm model load");
            }
            if !self
                .lms_models()?
                .as_array()
                .context("invalid lms ps response")?
                .is_empty()
            {
                bail!("LM Studio already has a model loaded outside sm");
            }
        } else {
            let mut cmd = Command::new(expand_home(&self.config.mtplx_path));
            cmd.arg("--version");
            if command_output(cmd, Duration::from_secs(5))?.trim() != "mtplx 2.12.0" {
                bail!("sm requires MTPLX 2.12.0; upgrade qualification is deliberate");
            }
            if self.pane()?.is_some()
                || std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], self.port)),
                    Duration::from_secs(1),
                )
                .is_ok()
            {
                bail!("a server already exists outside this load; refusing to take it over");
            }
        }
        Ok(())
    }
    fn start(&self, m: &ModelRecord) -> Result<Option<i32>> {
        if m.server == "lmstudio" {
            let mut cmd = Command::new(expand_home(&self.config.lms_path));
            cmd.args([
                "load",
                &m.key,
                "-c",
                &m.context.to_string(),
                "--parallel",
                &m.seats.to_string(),
                "--identifier",
                &m.identifier,
                "-y",
            ]);
            command_output(cmd, Duration::from_secs(600))?;
            return Ok(None);
        }
        // Dead panes contain logs only; killing one cannot kill a model.
        let _ = self.tmux(&["kill-session", "-t", "model"]);
        let cap = if m.seats == 2 {
            32 * GB
        } else {
            self.config.session_bank_max_bytes
        };
        let args = vec![
            expand_home(&self.config.mtplx_path)
                .to_string_lossy()
                .into_owned(),
            "serve".into(),
            "--model".into(),
            m.key.clone(),
            "--profile".into(),
            "turbo".into(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            self.port.to_string(),
            "--context-window".into(),
            m.context.to_string(),
            "--max-active-requests".into(),
            (m.seats + 1).to_string(),
            "--batching-preset".into(),
            "agent".into(),
            "--api-key".into(),
            self.config.auth_token.clone(),
            "--yes".into(),
        ];
        let command = format!(
            "MTPLX_SESSION_BANK_MAX_BYTES={cap} exec {}",
            args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
        );
        // Set remain-on-exit before launching, preserving cold-start failures.
        self.tmux(&["new-session", "-d", "-s", "model", "/bin/sh"])?;
        self.tmux(&["set-option", "-t", "model", "remain-on-exit", "on"])?;
        self.tmux(&["respawn-pane", "-k", "-t", "model", &command])?;
        self.pane()
    }
    fn ready(&self, m: &ModelRecord) -> Result<bool> {
        if m.server == "mtplx" && self.pane()?.is_none() {
            bail!("MTPLX exited before readiness; inspect its private tmux model pane");
        }
        Ok(self
            .models()
            .ok()
            .is_some_and(|v| v["data"].as_array().is_some_and(|a| !a.is_empty())))
    }
    fn stop(&self, m: &ModelRecord) -> Result<()> {
        if m.server == "lmstudio" {
            if self.lms_has(&m.identifier)? {
                let mut cmd = Command::new(expand_home(&self.config.lms_path));
                cmd.args(["unload", &m.identifier]);
                command_output(cmd, Duration::from_secs(180))?;
            }
            if self.lms_has(&m.identifier)? {
                bail!("LM Studio model still loaded after unload");
            }
            return Ok(());
        }
        if let Some(pid) = self.pane()? {
            if m.pid != Some(pid) {
                bail!("model pane ownership changed; unload refused");
            }
            // MTPLX's public stop CLI probes unauthenticated health and trusts
            // its reported PID. Signal only our verified private pane instead.
            // A graceful timeout keeps admission blocked; never escalate to KILL.
            terminate_owned_model(pid, Duration::from_secs(180))?;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.pane()?.is_some() {
            if Instant::now() >= deadline {
                bail!("MTPLX process still exists after stop; admission stays blocked");
            }
            thread::sleep(Duration::from_millis(100));
        }
        if self.models().is_ok() {
            bail!("model endpoint still responds after stop; admission stays blocked");
        }
        Ok(())
    }
    fn running(&self, m: &ModelRecord) -> Result<bool> {
        if m.server == "lmstudio" {
            self.lms_has(&m.identifier)
        } else {
            // An endpoint still answering after pane exit keeps admission
            // blocked, just as it does in stop().
            Ok(self.pane()?.is_some() || self.models().is_ok())
        }
    }
    fn owned_pid(&self, m: &ModelRecord) -> Result<Option<i32>> {
        if m.server == "mtplx" {
            self.pane()
        } else {
            Ok(None)
        }
    }
    fn footprint(&self, m: &ModelRecord) -> Result<Option<i64>> {
        let mut cmd = Command::new("/bin/ps");
        cmd.args(["-axo", "pid=,ppid=,command="]);
        let listing = command_output(cmd, Duration::from_secs(2))?;
        let roots = if m.server == "mtplx" {
            self.pane()?.into_iter().collect()
        } else {
            listing
                .lines()
                .filter(|l| {
                    l.contains("/.lmstudio/")
                        && (l.contains("llm-engine") || l.contains("mlx-engine"))
                })
                .filter_map(|l| l.split_whitespace().next()?.parse().ok())
                .collect()
        };
        let pids = descendants(&listing, roots);
        if pids.is_empty() {
            return Ok(None);
        }
        #[cfg(target_os = "macos")]
        {
            let readings: Option<Vec<i64>> = pids
                .into_iter()
                .map(crate::utilization::mac::phys_footprint)
                .collect();
            Ok(readings.map(|r| r.into_iter().sum()))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = pids;
            Ok(None)
        }
    }
}
fn live_pane_pid(text: &str) -> Result<Option<i32>> {
    let mut parts = text.split_whitespace();
    match parts.next() {
        Some("1") => Ok(None),
        Some("0") => {
            let pid: i32 = parts.next().context("missing model pid")?.parse()?;
            if pid <= 0 {
                bail!("invalid owned model pid");
            }
            // tmux can lag the kernel exit. Only ESRCH proves absence; other
            // errors (including permission errors) keep admission blocked.
            // SAFETY: signal 0 observes a positive PID without signaling it.
            if unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return Ok(None);
            }
            Ok(Some(pid))
        }
        _ => bail!("invalid model pane status"),
    }
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn command_output(command: Command, timeout: Duration) -> Result<String> {
    let output =
        crate::child_output::output_with_timeout(command, timeout).map_err(anyhow::Error::msg)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "model command failed ({}): {}",
            output.status,
            if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}
fn terminate_owned_model(pid: i32, timeout: Duration) -> Result<()> {
    if pid <= 0 {
        bail!("invalid owned model pid");
    }
    #[cfg(target_os = "macos")]
    let identity = crate::local_sockets::identity::ProcessIdentity::capture(pid as u32)
        .context("cannot verify owned model kernel identity")?;
    let alive = || {
        #[cfg(target_os = "macos")]
        {
            identity.is_live()
        }
        #[cfg(not(target_os = "macos"))]
        {
            // The caller has already matched this PID against its private pane.
            // SAFETY: signal 0 only checks the positive PID's existence.
            unsafe {
                libc::kill(pid, 0) == 0
                    || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
            }
        }
    };
    if !alive() {
        return Ok(());
    }
    // SAFETY: the caller verified the private pane PID, and the macOS kernel
    // identity was rechecked immediately above. Never signal a process group.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("cannot gracefully stop owned model");
        }
    }
    let deadline = Instant::now() + timeout;
    while alive() {
        if Instant::now() >= deadline {
            bail!("owned model did not exit after graceful stop; admission stays blocked");
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
fn descendants(listing: &str, mut roots: Vec<i32>) -> Vec<i32> {
    let pairs: Vec<(i32, i32)> = listing
        .lines()
        .filter_map(|l| {
            let mut p = l.split_whitespace();
            Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?))
        })
        .collect();
    loop {
        let before = roots.len();
        for (pid, parent) in &pairs {
            if roots.contains(parent) && !roots.contains(pid) {
                roots.push(*pid);
            }
        }
        if roots.len() == before {
            break;
        }
    }
    roots
}

static HOSTS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<ModelHost>>>> = OnceLock::new();
fn hosts() -> &'static Mutex<BTreeMap<PathBuf, Arc<ModelHost>>> {
    HOSTS.get_or_init(Mutex::default)
}
pub fn register_live(config: &AppConfig) -> Result<Arc<ModelHost>> {
    let db_path = expand_home(&config.sm_send.db_path);
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let queue_dir = expand_home(&config.queue_runner_state_dir().to_string_lossy());
    fs::create_dir_all(&queue_dir)?;
    let queue_dir = fs::canonicalize(queue_dir)?;
    let mut hosts = hosts()
        .lock()
        .map_err(|_| anyhow::anyhow!("local model registry poisoned"))?;
    if let Some(host) = hosts.get(&queue_dir) {
        return Ok(host.clone());
    }
    if !matches!(config.local_host.server.as_str(), "mtplx" | "lmstudio") {
        bail!("local_host.server must be mtplx or lmstudio");
    }
    let port = config
        .local_host
        .base_url
        .strip_prefix("http://127.0.0.1:")
        .and_then(|s| s.trim_end_matches('/').parse::<u16>().ok())
        .filter(|p| *p > 0)
        .context("local_host.base_url must be http://127.0.0.1:<port>")?;
    if config.local_host.session_bank_max_bytes <= 0 || config.local_host.yield_margin_bytes < 0 {
        bail!("local model memory limits must be positive");
    }
    let digest = Sha256::digest(db_path.to_string_lossy().as_bytes());
    let backend = CliServer {
        config: config.local_host.clone(),
        socket: format!("sm-model-{:x}", digest)[..25].into(),
        port,
    };
    let host = Arc::new(ModelHost {
        config: config.local_host.clone(),
        db_path: db_path.clone(),
        state_file: expand_home(&config.paths.state_file),
        queue_dir: queue_dir.clone(),
        queue_policy: config.queue_admission_policy(),
        operation: Mutex::new(()),
        yield_worker: AtomicBool::new(false),
        force_unload: AtomicBool::new(false),
        backend: Arc::new(backend),
    });
    // Durable ownership survives an sm restart. Do not silently claim a
    // different backend while a model is resident.
    if let Some(m) = host.record()? {
        if m.resident() && (m.server != host.config.server || m.endpoint != host.config.base_url) {
            bail!("unload the persisted local model before changing server configuration");
        }
    }
    host.recover()?;
    hosts.insert(queue_dir, host.clone());
    hosts.insert(fs::canonicalize(db_path)?, host.clone());
    Ok(host)
}
pub fn live(state_dir: &Path) -> Option<Arc<ModelHost>> {
    hosts()
        .lock()
        .ok()?
        .get(&fs::canonicalize(state_dir).ok()?)
        .cloned()
}
pub fn hold_perf(state_dir: &Path, label: &str) -> Result<bool> {
    live(state_dir).map_or(Ok(false), |h| {
        h.request_yield(format!("perf {label}"), false)
    })
}
pub fn guard_model(state_dir: &Path, host: Option<(i64, i64)>, reserve: i64) -> Result<bool> {
    let Some(model) = live(state_dir) else {
        return Ok(false);
    };
    let Some((_, available)) = host else {
        return Ok(false);
    };
    if available >= reserve.saturating_add(model.config.yield_margin_bytes) {
        return Ok(false);
    }
    model.request_yield("host memory pressure".into(), available < reserve)
}
pub fn sample_model(db_path: &Path) -> Result<Option<i64>> {
    live(db_path).map_or(Ok(None), |h| h.sample())
}

#[cfg(test)]
mod tests;
