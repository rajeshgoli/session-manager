//! Readers belong to the serving generation and never infer activity from panes.
use super::*;
use crate::{
    opencode::{
        events::{read_event, Activity},
        Client, OpencodeConfig,
    },
    sessions::OpencodeEventInput,
};
use anyhow::{bail, Result};
use std::{
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    sync::Weak,
};

#[derive(Default)]
struct ReaderState {
    connected_at: Option<Instant>,
    conversation: Option<String>,
    activity: Option<&'static str>,
}

#[derive(Clone, Default)]
pub(super) struct Readers {
    started: Arc<AtomicBool>,
    workers: Arc<Mutex<BTreeMap<String, ReaderState>>>,
}

impl Readers {
    pub(super) fn activity(&self, session: &SessionRecord) -> Option<&'static str> {
        let workers = self.workers.lock().ok()?;
        let status = workers.get(&session.id)?;
        if status.conversation.as_deref() != session.provider_resume_id.as_deref()
            || status.connected_at?.elapsed() > Duration::from_secs(60)
        {
            return None;
        }
        status.activity
    }
    fn update(&self, store: &SessionStore, id: &str) -> Result<()> {
        let session = store
            .get_session(id)?
            .context("reader session disappeared")?;
        if let Some(status) = self
            .workers
            .lock()
            .map_err(|_| anyhow::anyhow!("reader lock poisoned"))?
            .get_mut(id)
        {
            status.connected_at = Some(Instant::now());
            status.conversation = session.provider_resume_id;
            status.activity = match session.status.as_str() {
                "running" => Some("working"),
                "idle" => Some("idle"),
                _ => None,
            };
        }
        Ok(())
    }
    fn start_session(
        &self,
        state: &Arc<AppState>,
        handle: &tokio::runtime::Handle,
        id: String,
    ) -> Result<()> {
        let mut workers = self
            .workers
            .lock()
            .map_err(|_| anyhow::anyhow!("reader lock poisoned"))?;
        if workers.contains_key(&id) {
            return Ok(());
        }
        workers.insert(id.clone(), ReaderState::default());
        drop(workers);
        let readers = self.clone();
        let weak = Arc::downgrade(state);
        let handle = handle.clone();
        let worker_id = id.clone();
        if let Err(error) = thread::Builder::new()
            .name(format!("sm-opencode-events-{id}"))
            .spawn(move || {
                let _claim = ReaderClaim {
                    readers: readers.clone(),
                    id: worker_id.clone(),
                };
                let mut delay = 1;
                while let Some(state) = weak.upgrade() {
                    if !continue_reading(&state, &worker_id).unwrap_or(false) {
                        break;
                    }
                    let result = read_connection(&state, &readers, &handle, &worker_id);
                    if state.shutdown.is_stopped() {
                        break;
                    }
                    if let Err(error) = &result {
                        eprintln!("opencode reader {worker_id}: {error:#}");
                    }
                    let wait = if result.is_ok() {
                        delay = 1;
                        1
                    } else {
                        let wait = delay;
                        delay = (delay * 2).min(5);
                        wait
                    };
                    interruptible_wait(&state.shutdown, Duration::from_secs(wait));
                }
            })
        {
            self.workers
                .lock()
                .map_err(|_| anyhow::anyhow!("reader lock poisoned"))?
                .remove(&id);
            return Err(error.into());
        }
        Ok(())
    }
}
struct ReaderClaim {
    readers: Readers,
    id: String,
}
impl Drop for ReaderClaim {
    fn drop(&mut self) {
        if let Ok(mut workers) = self.readers.workers.lock() {
            workers.remove(&self.id);
        }
    }
}

pub(super) fn start(state: Arc<AppState>) {
    if !state.config.rust_core.runtime_enabled
        || state.opencode_readers.started.swap(true, Ordering::AcqRel)
    {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        state
            .opencode_readers
            .started
            .store(false, Ordering::Release);
        eprintln!("opencode readers need the server runtime");
        return;
    };
    let weak: Weak<AppState> = Arc::downgrade(&state);
    let readers = state.opencode_readers.clone();
    let started = readers.started.clone();
    if let Err(error) = thread::Builder::new()
        .name("sm-opencode-readers".into())
        .spawn(move || {
            while let Some(state) = weak.upgrade() {
                if state.shutdown.is_stopped() {
                    break;
                }
                match state.session_store.list_sessions(true) {
                    Ok(sessions) => {
                        for session in sessions
                            .into_iter()
                            .filter(|s| s.provider == "opencode" && is_primary_node(&s.node))
                        {
                            if session.is_stopped() {
                                if let Err(error) =
                                    state.session_store.recover_opencode_effects(&session.id)
                                {
                                    eprintln!(
                                        "opencode retained effects {}: {error:#}",
                                        session.id
                                    );
                                }
                            } else if let Err(error) =
                                readers.start_session(&state, &handle, session.id)
                            {
                                eprintln!("opencode reader start: {error:#}");
                            }
                        }
                    }
                    Err(error) => eprintln!("opencode reader reconciliation: {error:#}"),
                }
                interruptible_wait(&state.shutdown, Duration::from_secs(1));
            }
            started.store(false, Ordering::Release);
        })
    {
        state
            .opencode_readers
            .started
            .store(false, Ordering::Release);
        eprintln!("opencode reader supervisor start: {error}");
    }
}
fn interruptible_wait(shutdown: &crate::handover::Shutdown, duration: Duration) {
    let until = Instant::now() + duration;
    while !shutdown.is_stopped() && Instant::now() < until {
        thread::sleep(
            until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        );
    }
}
fn continue_reading(state: &AppState, id: &str) -> Result<bool> {
    if state.shutdown.is_stopped() {
        return Ok(false);
    }
    Ok(state
        .session_store
        .get_session(id)?
        .is_some_and(|s| s.provider == "opencode" && !s.is_stopped()))
}
fn private_file(path: &StdPath, limit: u64) -> Result<Vec<u8>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > limit
    {
        bail!("opencode reader artifact is not private and independent")
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("opencode reader artifact exceeds its size limit")
    }
    Ok(bytes)
}
fn client_and_config(
    state: &AppState,
    session: &SessionRecord,
) -> Result<(Client, OpencodeConfig)> {
    let binding = session
        .opencode
        .as_ref()
        .context("opencode runtime binding missing")?;
    binding.validate()?;
    if session.id.is_empty()
        || session.id.len() > 64
        || !session
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        bail!("invalid opencode reader session ID")
    }
    let root = expand_home(&state.config.opencode.state_root).canonicalize()?;
    let expected = root.join(&session.id);
    if expected != StdPath::new(&binding.state_dir) || expected.canonicalize()? != expected {
        bail!("opencode reader state is outside its physical agent directory")
    }
    let secret = private_file(&expected.join("server.secret"), 64)?;
    if secret.len() != 64 || !secret.iter().all(u8::is_ascii_hexdigit) {
        bail!("invalid opencode server secret")
    }
    let config_json: Value = serde_json::from_slice(&private_file(
        &expected.join("xdg/config/opencode/opencode.json"),
        1024 * 1024,
    )?)?;
    let model = config_json["model"]
        .as_str()
        .and_then(|v| v.strip_prefix("local/"))
        .context("opencode local model missing")?;
    let model_spec = &config_json["provider"]["local"]["models"][model];
    let config = OpencodeConfig {
        model_id: model.into(),
        model_base_url: binding.model_base_url.clone(),
        context_window: model_spec["limit"]["context"]
            .as_u64()
            .context("opencode context window missing")?,
        output_limit: model_spec["limit"]["output"]
            .as_u64()
            .context("opencode output limit missing")?,
        ..state.config.opencode.clone()
    };
    config.validate()?;
    Ok((
        Client::new(
            binding.port,
            std::str::from_utf8(&secret)?,
            Duration::from_secs(5),
        )?,
        config,
    ))
}
fn dispatch_stop(state: &Arc<AppState>, handle: &tokio::runtime::Handle, id: &str) -> Result<()> {
    if state.shutdown.is_stopped() {
        return Ok(());
    }
    if let Some(signal) = state.session_store.opencode_pending_stop_signal(id)? {
        let _runtime = handle.enter();
        handoff::kick_handoff(state.clone(), id.into());
        state
            .session_store
            .acknowledge_opencode_stop_signal(id, &signal)?;
    }
    Ok(())
}
fn resync(
    state: &Arc<AppState>,
    handle: &tokio::runtime::Handle,
    id: &str,
    client: &Client,
    config: &OpencodeConfig,
) -> Result<String> {
    let session = state
        .session_store
        .get_session(id)?
        .context("opencode session disappeared")?;
    let conversation = session
        .provider_resume_id
        .context("opencode conversation missing")?;
    let mut activity = current_activity(client, &conversation)?;
    let mut snapshot = None;
    for _ in 0..3 {
        let messages = client.conversation_messages(&conversation)?;
        let after = current_activity(client, &conversation)?;
        if activity == after {
            snapshot = Some(messages);
            break;
        }
        activity = after;
    }
    // Idle is observed before history, and confirmed afterwards. A busy/idle
    // transition during the GET must retry rather than stop with old text.
    let messages = snapshot.context("opencode activity changed during history snapshot")?;
    if state.shutdown.is_stopped() {
        bail!("opencode reader stopping")
    }
    let generated = state
        .session_store
        .opencode_generated_message_ids(id, &conversation)?;
    state.session_store.apply_opencode_events(
        id,
        OpencodeEventInput::Backfill {
            conversation: &conversation,
            messages: &messages,
            activity,
        },
        config,
        &generated,
        &expand_home(&state.config.tool_logging.db_path),
    )?;
    dispatch_stop(state, handle, id)?;
    // #2045 supplies HTTP outbox delivery. Never type into the native viewer.
    Ok(conversation)
}
fn current_activity(client: &Client, conversation: &str) -> Result<Activity> {
    client
        .status()?
        .get(conversation)
        .map(Activity::from_value)
        .transpose()
        .map(|activity| activity.unwrap_or(Activity::Idle))
}
fn read_connection(
    state: &Arc<AppState>,
    readers: &Readers,
    handle: &tokio::runtime::Handle,
    id: &str,
) -> Result<()> {
    if !continue_reading(state, id)? {
        return Ok(());
    }
    state.session_store.recover_opencode_effects(id)?;
    dispatch_stop(state, handle, id)?;
    let session = state
        .session_store
        .get_session(id)?
        .context("opencode session disappeared")?;
    let (client, config) = client_and_config(state, &session)?;
    let mut stream = client.event_stream()?;
    let connected = Instant::now();
    let mut conversation = resync(state, handle, id, &client, &config)?;
    readers.update(&state.session_store, id)?;
    loop {
        let event = match read_event(&mut stream) {
            Ok(Some(event)) => event,
            Ok(None) => break,
            // The stream body deliberately rotates at twenty seconds.
            Err(_) if connected.elapsed() >= Duration::from_secs(20) => break,
            Err(error) => return Err(error),
        };
        if !continue_reading(state, id)? {
            break;
        }
        let session = state
            .session_store
            .get_session(id)?
            .context("opencode session disappeared")?;
        if session.provider_resume_id.as_deref() != Some(&conversation) {
            conversation = resync(state, handle, id, &client, &config)?;
        }
        // Lifecycle frames buffered before the snapshot must not revert it.
        // Refresh history as well so idle always carries the full final reply,
        // even when its last text/metadata frame is still buffered behind us.
        if event["type"] == "session.status"
            && event["properties"]["sessionID"].as_str() == Some(&conversation)
        {
            conversation = resync(state, handle, id, &client, &config)?;
            readers.update(&state.session_store, id)?;
            continue;
        }
        if state.shutdown.is_stopped() {
            break;
        }
        let generated = state
            .session_store
            .opencode_generated_message_ids(id, &conversation)?;
        state.session_store.apply_opencode_events(
            id,
            OpencodeEventInput::Live(&event),
            &config,
            &generated,
            &expand_home(&state.config.tool_logging.db_path),
        )?;
        dispatch_stop(state, handle, id)?;
        readers.update(&state.session_store, id)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "opencode_readers_tests.rs"]
mod tests;
