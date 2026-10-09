//! Durable orchestration. Host/provider waits never hold the registry lock.
use super::*;
use crate::opencode::{Client, DeliveryOutcome, OpencodeConfig, RuntimeBinding};

pub trait OpencodeLaunchDriver: Send + Sync {
    fn requires_reader(&self) -> bool {
        false
    }
    fn base_config(&self) -> OpencodeConfig;
    fn config(&self, requested: Option<&str>) -> Result<OpencodeConfig>;
    fn binding(&self, config: &OpencodeConfig, id: &str, port: u16) -> Result<RuntimeBinding>;
    fn start(
        &self,
        config: &OpencodeConfig,
        record: &SessionRecord,
        credential: &str,
        runtime: &TmuxRuntime,
    ) -> Result<()>;
    fn client(&self, binding: &RuntimeBinding) -> Result<Client>;
    fn attach(&self, record: &SessionRecord, runtime: &TmuxRuntime) -> Result<()>;
    fn replace_attach(&self, record: &SessionRecord, runtime: &TmuxRuntime) -> Result<()> {
        self.attach(record, runtime)
    }
    fn pause_attach(&self, record: &SessionRecord, runtime: &TmuxRuntime) -> Result<()>;
    fn present(&self, record: &SessionRecord, runtime: &TmuxRuntime) -> Result<bool>;
    fn stop(&self, record: &SessionRecord, runtime: &TmuxRuntime) -> Result<()>;
}

#[cfg(test)]
#[path = "session_launch_tests.rs"]
mod tests;

type ReaderStart = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;
#[derive(Clone)]
pub struct OpencodeLaunchContext {
    driver: Arc<dyn OpencodeLaunchDriver>,
    reader_start: Arc<Mutex<Option<ReaderStart>>>,
}
impl std::fmt::Debug for OpencodeLaunchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OpencodeLaunchContext")
    }
}

impl SessionStore {
    /// Read-only admission hint; creation repeats the check under its reservation lock.
    pub fn opencode_capacity_reason(&self, config: &OpencodeConfig) -> Result<Option<String>> {
        let occupied = occupied_opencode_sessions(&self.load_parsed_state()?.raw, None)?;
        Ok(check_opencode_capacity(config, &occupied, None)
            .err()
            .map(|error| error.to_string()))
    }

    pub fn with_opencode_launch_driver(mut self, driver: Arc<dyn OpencodeLaunchDriver>) -> Self {
        self.opencode_launch = Some(OpencodeLaunchContext {
            driver,
            reader_start: Arc::new(Mutex::new(None)),
        });
        self
    }
    pub(super) fn opencode_driver(&self) -> Result<Arc<dyn OpencodeLaunchDriver>> {
        self.opencode_launch
            .as_ref()
            .map(|context| context.driver.clone())
            .context("opencode host launcher is not registered")
    }

    pub(crate) fn register_opencode_reader_start(&self, start: ReaderStart) -> Result<()> {
        if let Some(context) = &self.opencode_launch {
            *context
                .reader_start
                .lock()
                .map_err(|_| anyhow::anyhow!("opencode reader registration lock poisoned"))? =
                Some(start);
        }
        Ok(())
    }
    fn start_opencode_reader(&self, id: &str, driver: &dyn OpencodeLaunchDriver) -> Result<()> {
        let context = self
            .opencode_launch
            .as_ref()
            .context("opencode host launcher missing")?;
        let start = context
            .reader_start
            .lock()
            .map_err(|_| anyhow::anyhow!("opencode reader registration lock poisoned"))?
            .clone();
        match start {
            Some(start) => start(id),
            None if driver.requires_reader() => {
                anyhow::bail!("opencode reader generation is not registered")
            }
            None => Ok(()),
        }
    }

    /// Both submission and retirement acquire this before the registry lock.
    /// Provider waits may hold it without preventing unrelated registry work.
    pub(super) fn lock_opencode_submission(&self, id: &str) -> Result<SessionClearGuard> {
        self.lock_named_clear_operation(&format!("opencode-submission:{id}"))
    }

    pub fn create_opencode_session_with_runtime(
        &self,
        request: CreateCoreSessionRequest,
        log_dir: Option<PathBuf>,
        runtime: &TmuxRuntime,
    ) -> Result<SessionRecord> {
        self.create_opencode_session(request, log_dir, runtime, None)
    }
    /// Only the server's handoff path supplies a predecessor; it is not part
    /// of CreateCoreSessionRequest and cannot be selected by an HTTP caller.
    pub fn create_opencode_handoff_successor(
        &self,
        request: CreateCoreSessionRequest,
        log_dir: Option<PathBuf>,
        runtime: &TmuxRuntime,
        predecessor: &str,
    ) -> Result<SessionRecord> {
        self.create_opencode_session(request, log_dir, runtime, Some(predecessor))
    }
    fn create_opencode_session(
        &self,
        mut request: CreateCoreSessionRequest,
        log_dir: Option<PathBuf>,
        runtime: &TmuxRuntime,
        predecessor: Option<&str>,
    ) -> Result<SessionRecord> {
        let driver = self.opencode_driver()?;
        let max_wait = request.max_wait_seconds.unwrap_or(300);
        if max_wait == 0 {
            anyhow::bail!("max_wait_seconds must be greater than 0");
        }
        let deadline = std::time::Instant::now()
            .checked_add(Duration::from_secs(max_wait))
            .context("max_wait_seconds is too large")?;
        let waiting_config = driver.config(request.model.as_deref())?;
        let (config, _admission) = loop {
            let admission = self.lock_named_clear_operation("local-seat-admission")?;
            let config = &waiting_config;
            let capacity = {
                let _guard = self.write_guard()?;
                check_opencode_capacity(
                    config,
                    &occupied_opencode_sessions(&self.load_parsed_state()?.raw, None)?,
                    predecessor,
                )
            };
            match capacity {
                // Recheck the model once admission succeeds, without running
                // the provider version probe on every seat-wait poll.
                Ok(()) => break (driver.config(request.model.as_deref())?, admission),
                Err(error)
                    if error
                        .downcast_ref::<crate::opencode::launch::AdmissionError>()
                        .is_some()
                        && std::time::Instant::now() < deadline =>
                {
                    drop(admission);
                    thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error),
            }
        };
        config.validate()?;
        let id = request
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .map(Ok)
            .unwrap_or_else(|| {
                generate_unique_session_id(&sessions_with_registered_aliases(
                    &self.load_parsed_state()?.raw,
                ))
            })?;
        request.id = Some(id.clone());
        // Recovery uses this same lock. Acquire it before publishing any
        // provisional record so a reader cannot recover a half-started launch.
        let _launch_guard = self.lock_clear_operation(&id)?;
        let credential = generate_session_credential();
        let (record, launch) = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            if let Some(parent) = request.parent_session_id.as_deref() {
                ensure_session_not_reparent_fenced(&state, parent)?;
            }
            let mut record = self.build_core_session_record(
                &sessions_with_registered_aliases(&state),
                &request,
                log_dir.as_deref(),
                true,
                runtime.socket_name(),
            )?;
            if record.provider != "opencode" {
                anyhow::bail!("opencode launch requires the opencode provider");
            }
            ensure_runtime_local_node(&record.node)?;
            let occupied = occupied_opencode_sessions(&state, None)?;
            check_opencode_capacity(&config, &occupied, predecessor)?;
            let port = reserve_opencode_port(&config, &occupied)?;
            record.opencode = Some(driver.binding(&config, &record.id, port)?);
            record.model = Some(config.model_id.clone());
            record.reasoning_effort = None;
            record.host = Some("local".into());
            record.status = "starting".into();
            record.stopped_at = None;
            record.session_credential_sha256 = Some(sha256_text(&credential));
            let mut launches = session_runtime_launch_records(&state)?;
            let launch = opencode_launch_record(
                &record,
                &request,
                generate_unique_runtime_launch_id(&launches)?,
                "create",
            );
            launches.push(launch.clone());
            ensure_sessions_array_mut(&mut state)?.push(serde_json::to_value(&record)?);
            if let Some(brief) = request.spawn_brief.as_ref() {
                bind_spawn_launch_intent_in_state(&mut state, &brief.intent_id, &record.id)?;
            }
            store_session_runtime_launch_records(&mut state, &launches)?;
            self.write_raw_json_value(&state)?;
            (record, launch)
        };
        drop(_admission);
        let result = (|| {
            driver.start(&config, &record, &credential, runtime)?;
            let client = driver.client(
                record
                    .opencode
                    .as_ref()
                    .context("missing opencode binding")?,
            )?;
            let conversation = client
                .create_conversation(record.friendly_name.as_deref().unwrap_or(&record.name))?;
            self.bind_opencode_launch_conversation(&launch.id, &conversation)?;
            let current = self
                .get_session(&record.id)?
                .context("launch session disappeared")?;
            self.start_opencode_reader(&record.id, driver.as_ref())?;
            driver.attach(&current, runtime)?;
            self.finish_opencode_launch(&launch.id, runtime, driver.as_ref())
        })();
        self.handle_opencode_launch_result(&launch, &record, runtime, driver.as_ref(), result)
    }

    pub fn restore_opencode_session_with_runtime(
        &self,
        id: &str,
        runtime: &TmuxRuntime,
    ) -> Result<Option<CoreRestoreOutcome>> {
        let _launch_guard = self.lock_clear_operation(id)?;
        let driver = self.opencode_driver()?;
        let (original, expected_authority) = {
            let _guard = self.write_guard()?;
            let Some(original) = self.get_session(id)? else {
                return Ok(None);
            };
            let state = self.load_parsed_state()?;
            let authority = opencode_restore_authority(&state.raw, &original.id)?;
            (original, authority)
        };
        if !is_primary_node(&original.node) {
            return Ok(Some(CoreRestoreOutcome::UnsupportedNode(original.node)));
        }
        if original.provider != "opencode" {
            return Ok(Some(CoreRestoreOutcome::UnsupportedProvider(
                original.provider,
            )));
        }
        if !original.is_stopped() && driver.present(&original, runtime)? {
            return Ok(Some(CoreRestoreOutcome::NotStopped));
        }
        let Some(conversation) = original.provider_resume_id.clone() else {
            return Ok(Some(CoreRestoreOutcome::MissingProviderResumeId(
                "opencode".into(),
            )));
        };
        {
            let _guard = self.write_guard()?;
            let state = self.load_parsed_state()?;
            ensure_session_not_reparent_fenced(&state.raw, id)?;
            if raw_session_object(&state.raw, id).is_some_and(|r| {
                r.get("opencode_pending_retire")
                    .is_some_and(|v| !v.is_null())
            }) {
                anyhow::bail!("opencode retirement is awaiting verified teardown; restore refused")
            }
            if session_runtime_launch_records(&state.raw)?
                .iter()
                .any(|launch| launch.session_id == id && opencode_launch_pending(&launch.status))
            {
                anyhow::bail!(
                    "opencode launch is still pending; recover its saved brief before restore"
                );
            }
        }
        let config = driver.config(None)?;
        config.validate()?;
        check_opencode_capacity(
            &config,
            &occupied_opencode_sessions(&self.load_parsed_state()?.raw, Some(id))?,
            None,
        )?;
        // Teardown must be proved before replacing immutable wall settings or
        // making the newly rotated credential authoritative.
        driver.stop(&original, runtime)?;
        let credential = generate_session_credential();
        let (record, launch) = {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            ensure_session_not_reparent_fenced(&state, id)?;
            // Retirement does not take the clear lock. Its terminal write
            // while host teardown waits must win over this older restore.
            if opencode_restore_authority(&state, id)? != expected_authority {
                anyhow::bail!("session lifecycle changed during opencode restore; relaunch refused")
            }
            let occupied = occupied_opencode_sessions(&state, Some(id))?;
            check_opencode_capacity(&config, &occupied, None)?;
            let port = reserve_opencode_port(&config, &occupied)?;
            let mut record = self
                .get_session(id)?
                .context("restore session disappeared")?;
            if record.provider_resume_id.as_deref() != Some(&conversation) {
                anyhow::bail!("conversation changed during restore")
            }
            let new_name = record
                .friendly_name
                .as_deref()
                .or(Some(record.name.as_str()))
                .and_then(|name| {
                    restored_name(&sessions_with_registered_aliases(&state), id, name)
                });
            let mut renamed = serde_json::to_value(&record)?;
            apply_restored_name(
                renamed.as_object_mut().context("invalid restore session")?,
                new_name,
            );
            record = serde_json::from_value(renamed)?;
            record.opencode = Some(driver.binding(&config, id, port)?);
            record.model = Some(config.model_id.clone());
            record.reasoning_effort = None;
            record.host = Some("local".into());
            record.status = "starting".into();
            record.stopped_at = None;
            record.completion_status = None;
            record.completion_message = None;
            record.completed_at = None;
            record.terminal_provenance = None;
            record.agent_task_completed_at = None;
            record.session_credential_sha256 = Some(sha256_text(&credential));
            let request = CreateCoreSessionRequest {
                provider: Some("opencode".into()),
                ..CreateCoreSessionRequest::default()
            };
            let mut launches = session_runtime_launch_records(&state)?;
            let mut launch = opencode_launch_record(
                &record,
                &request,
                generate_unique_runtime_launch_id(&launches)?,
                "restore",
            );
            launch.opencode_restore_terminal_metadata =
                Some(opencode_terminal_metadata(&state, id)?);
            launches.push(launch.clone());
            let session = session_object_mut(ensure_sessions_array_mut(&mut state)?, id)
                .context("restore session disappeared")?;
            // Preserve raw fields such as the projection/context checkpoint.
            for (key, value) in serde_json::to_value(&record)?
                .as_object()
                .context("invalid session")?
            {
                session.insert(key.clone(), value.clone());
            }
            session.remove("error_message");
            session.insert("retirement_intent".into(), Value::Null);
            store_session_runtime_launch_records(&mut state, &launches)?;
            self.write_raw_json_value(&state)?;
            (record, launch)
        };
        let result = (|| {
            driver.start(&config, &record, &credential, runtime)?;
            let client = driver.client(
                record
                    .opencode
                    .as_ref()
                    .context("missing opencode binding")?,
            )?;
            client.conversation_messages(&conversation)?;
            self.start_opencode_reader(&record.id, driver.as_ref())?;
            driver.attach(&record, runtime)?;
            self.finish_opencode_launch(&launch.id, runtime, driver.as_ref())
        })();
        self.handle_opencode_launch_result(&launch, &record, runtime, driver.as_ref(), result)
            .map(|record| Some(CoreRestoreOutcome::Restored(Box::new(record))))
    }

    fn bind_opencode_launch_conversation(&self, launch_id: &str, conversation: &str) -> Result<()> {
        crate::opencode::validate_id(conversation, "ses")?;
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut launches = session_runtime_launch_records(&state)?;
        let launch = launches
            .iter_mut()
            .find(|launch| launch.id == launch_id)
            .context("opencode launch disappeared")?;
        if launch.status != "launching" || launch.provider != "opencode" {
            anyhow::bail!("opencode launch is no longer active")
        }
        let session = raw_session_object(&state, &launch.session_id)
            .context("opencode session disappeared")?;
        if json_text(session.get("session_credential_sha256")).as_deref()
            != Some(&launch.credential_sha256)
            || completion_status_is_retired(
                session.get("completion_status").and_then(Value::as_str),
            )
            || session_restore_is_fenced(&state, &launch.session_id)
        {
            anyhow::bail!("opencode launch lost lifecycle authority")
        }
        launch.provider_resume_id = Some(conversation.into());
        if launch
            .initial_message
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        {
            launch.ensure_opencode_brief_binding()?;
        }
        let id = launch.session_id.clone();
        store_session_runtime_launch_records(&mut state, &launches)?;
        session_object_mut(ensure_sessions_array_mut(&mut state)?, &id)
            .context("opencode session disappeared")?
            .insert("provider_resume_id".into(), json!(conversation));
        self.write_raw_json_value(&state)
    }
    fn finish_opencode_launch(
        &self,
        launch_id: &str,
        runtime: &TmuxRuntime,
        driver: &dyn OpencodeLaunchDriver,
    ) -> Result<SessionRecord> {
        let launch = session_runtime_launch_records(&self.load_parsed_state()?.raw)?
            .into_iter()
            .find(|launch| launch.id == launch_id)
            .context("opencode launch disappeared")?;
        let record = self
            .get_session(&launch.session_id)?
            .context("opencode session disappeared")?;
        if !driver.present(&record, runtime)? {
            anyhow::bail!("opencode runtime disappeared before launch acknowledgement")
        }
        let client = driver.client(
            record
                .opencode
                .as_ref()
                .context("missing opencode binding")?,
        )?;
        if !client.ready()? {
            anyhow::bail!("opencode server is not ready")
        }
        // Hold through acceptance and the applied commit. A terminal transition
        // either precedes the authority check or follows the completed send.
        let _submission_guard = self.lock_opencode_submission(&record.id)?;
        {
            let _guard = self.write_guard()?;
            let state = self.load_parsed_state()?;
            let current = session_runtime_launch_records(&state.raw)?
                .into_iter()
                .find(|current| current.id == launch_id)
                .context("launch disappeared")?;
            let raw = raw_session_object(&state.raw, &record.id).context("session disappeared")?;
            if current.status != "launching"
                || raw.get("provider_resume_id").and_then(Value::as_str)
                    != launch.provider_resume_id.as_deref()
                || raw.get("session_credential_sha256").and_then(Value::as_str)
                    != Some(&launch.credential_sha256)
                || raw.get("status").and_then(Value::as_str) == Some("stopped")
                || completion_status_is_retired(
                    raw.get("completion_status").and_then(Value::as_str),
                )
                || session_restore_is_fenced(&state.raw, &record.id)
            {
                anyhow::bail!("opencode launch lost lifecycle authority")
            }
        }
        let journal = PathBuf::from(
            &record
                .opencode
                .as_ref()
                .context("missing opencode binding")?
                .state_dir,
        )
        .join("usage.jsonl");
        self.seat_session_store.append(
            &record.id,
            "opencode",
            record
                .provider_resume_id
                .as_deref()
                .context("missing opencode conversation")?,
            Some(&journal.to_string_lossy()),
        )?;
        if let Some(text) = launch
            .initial_message
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            let mut bound = launch.clone();
            let binding = bound.ensure_opencode_brief_binding()?;
            if bound.brief_message_id != launch.brief_message_id
                || bound.brief_part_id != launch.brief_part_id
            {
                anyhow::bail!("brief identities were not persisted before delivery")
            }
            if !matches!(
                client.attempt_delivery(
                    &binding,
                    text,
                    Duration::from_secs(driver.base_config().confirm_timeout_secs)
                ),
                Ok(DeliveryOutcome::Accepted)
            ) {
                return Err(InitialBriefDeliveryError::ProviderAcceptanceTimedOut {
                    provider: "opencode".into(),
                }
                .into());
            }
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let mut launches = session_runtime_launch_records(&state)?;
        let current = launches
            .iter_mut()
            .find(|current| current.id == launch_id)
            .context("launch disappeared")?;
        let raw = raw_session_object(&state, &record.id).context("session disappeared")?;
        if current.status != "launching"
            || raw.get("provider_resume_id").and_then(Value::as_str)
                != launch.provider_resume_id.as_deref()
            || raw.get("session_credential_sha256").and_then(Value::as_str)
                != Some(&launch.credential_sha256)
            || completion_status_is_retired(raw.get("completion_status").and_then(Value::as_str))
            || session_restore_is_fenced(&state, &record.id)
        {
            anyhow::bail!("opencode launch lost lifecycle authority")
        }
        current.status = "applied".into();
        current.updated_at = now_rfc3339();
        store_session_runtime_launch_records(&mut state, &launches)?;
        let session = session_object_mut(ensure_sessions_array_mut(&mut state)?, &record.id)
            .context("session disappeared")?;
        // A reader may already have committed idle or running activity while
        // the provider confirms the brief. Do not overwrite that observation.
        if session.get("status").and_then(Value::as_str) == Some("starting") {
            session.insert(
                "status".into(),
                json!(if launch.initial_message.is_some() {
                    "running"
                } else {
                    "idle"
                }),
            );
        }
        session.insert("stopped_at".into(), Value::Null);
        session.insert("last_activity".into(), json!(now_rfc3339()));
        self.write_raw_json_value(&state)?;
        self.get_session(&record.id)?.context("session disappeared")
    }
    fn handle_opencode_launch_result(
        &self,
        launch: &SessionRuntimeLaunchRecord,
        record: &SessionRecord,
        runtime: &TmuxRuntime,
        driver: &dyn OpencodeLaunchDriver,
        result: Result<SessionRecord>,
    ) -> Result<SessionRecord> {
        match result {
            Ok(record) => Ok(record),
            Err(error) if error.chain().any(|cause| matches!(cause.downcast_ref::<InitialBriefDeliveryError>(), Some(InitialBriefDeliveryError::ProviderAcceptanceTimedOut { .. }))) => Err(error.context("opencode brief acceptance remains unknown; saved identities retained for recovery")),
            Err(error) => {
                let stopped = driver.stop(record, runtime);
                let reason = format!("{error:#}; state {}", record.opencode.as_ref().map(|binding| binding.state_dir.as_str()).unwrap_or("unavailable"));
                self.record_opencode_teardown_result(launch, record, &stopped, &reason)?;
                Err(error.context(reason))
            }
        }
    }
    fn record_opencode_teardown_result(
        &self,
        launch: &SessionRuntimeLaunchRecord,
        record: &SessionRecord,
        stopped: &Result<()>,
        reason: &str,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        if let Err(error) = stopped {
            let mut launches = session_runtime_launch_records(&state)?;
            let current = launches
                .iter_mut()
                .find(|current| current.id == launch.id)
                .context("launch disappeared during teardown")?;
            current.status = "teardown_pending".into();
            current.updated_at = now_rfc3339();
            current.failure_reason = Some(reason.into());
            store_session_runtime_launch_records(&mut state, &launches)?;
            if let Some(session) =
                session_object_mut(ensure_sessions_array_mut(&mut state)?, &record.id)
            {
                session.insert(
                    "error_message".into(),
                    json!(format!("{reason}; teardown not confirmed: {error:#}")),
                );
            }
        } else {
            let mut restore_metadata = launch
                .opencode_restore_terminal_metadata
                .as_ref()
                .filter(|_| launch.operation_kind == "restore")
                .filter(|_| {
                    raw_session_object(&state, &record.id).is_some_and(|session| {
                        session
                            .get("session_credential_sha256")
                            .and_then(Value::as_str)
                            == Some(launch.credential_sha256.as_str())
                            && session.get("provider_resume_id").and_then(Value::as_str)
                                == launch.provider_resume_id.as_deref()
                            && [
                                "completion_status",
                                "completion_message",
                                "completed_at",
                                "terminal_provenance",
                                "retirement_intent",
                                "agent_task_completed_at",
                            ]
                            .iter()
                            .all(|field| session.get(*field).is_none_or(Value::is_null))
                    })
                })
                .cloned();
            // The generic launch failure helper updates stopped_at. Preserve
            // the entire newer terminal decision, including its timestamp.
            if restore_metadata.is_none()
                && raw_session_object(&state, &record.id).is_some_and(|session| {
                    ["completion_status", "terminal_provenance"]
                        .iter()
                        .any(|field| session.get(*field).is_some_and(|value| !value.is_null()))
                })
            {
                restore_metadata = Some(opencode_terminal_metadata(&state, &record.id)?);
            }
            let remove = launch.operation_kind == "create"
                && remove_failed_provisional_runtime_session(&state, &record.id);
            mark_runtime_launch_failed(&mut state, &launch.id, &record.id, remove, reason)?;
            if let Some(metadata) = restore_metadata {
                let session =
                    session_object_mut(ensure_sessions_array_mut(&mut state)?, &record.id)
                        .context("restore session disappeared during teardown")?;
                for (field, value) in metadata
                    .as_object()
                    .context("invalid restore terminal metadata")?
                {
                    session.insert(field.clone(), value.clone());
                }
            }
        }
        self.write_raw_json_value(&state)
    }
    pub fn recover_opencode_launch_for_session(&self, id: &str) -> Result<()> {
        if self.opencode_launch.is_none() {
            return Ok(());
        }
        if raw_session_object(&self.load_parsed_state()?.raw, id).is_some_and(|r| {
            r.get("opencode_pending_retire")
                .is_some_and(|v| !v.is_null())
        }) {
            return Ok(());
        }
        let launch = session_runtime_launch_records(&self.load_parsed_state()?.raw)?
            .into_iter()
            .find(|launch| {
                launch.session_id == id
                    && launch.provider == "opencode"
                    && opencode_launch_pending(&launch.status)
            });
        if let Some(launch) = launch {
            self.recover_opencode_runtime_launch(&launch)?;
        }
        Ok(())
    }
    pub fn recover_opencode_runtime_launches(&self) -> Result<()> {
        if self.opencode_launch.is_none() {
            return Ok(());
        }
        let pending = session_runtime_launch_records(&self.load_parsed_state()?.raw)?;
        let mut first_error = None;
        for launch in pending.iter().filter(|launch| {
            launch.provider == "opencode" && opencode_launch_pending(&launch.status)
        }) {
            if let Err(error) = self.recover_opencode_runtime_launch(launch) {
                eprintln!("opencode launch {}: {error:#}", launch.id);
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) fn recover_opencode_teardowns(&self) -> Result<()> {
        if self.opencode_launch.is_none() {
            return Ok(());
        }
        for launch in session_runtime_launch_records(&self.load_parsed_state()?.raw)?
            .iter()
            .filter(|launch| launch.provider == "opencode" && launch.status == "teardown_pending")
        {
            if let Err(error) = self.recover_opencode_runtime_launch(launch) {
                eprintln!("opencode teardown {} deferred: {error:#}", launch.id);
            }
        }
        Ok(())
    }

    pub fn reconcile_opencode_runtime(&self, id: &str) -> Result<()> {
        if self.opencode_launch.is_none() {
            return Ok(());
        }
        let Some(record) = self
            .get_session(id)?
            .filter(|record| record.provider == "opencode" && !record.is_stopped())
        else {
            return Ok(());
        };
        // A restore publishes its new binding before preparing the native
        // server. The reader must not turn that reservation into a false exit.
        if session_runtime_launch_records(&self.load_parsed_state()?.raw)?
            .iter()
            .any(|launch| launch.session_id == id && opencode_launch_pending(&launch.status))
        {
            return Ok(());
        }
        let runtime = self
            .delivery_runtime
            .as_ref()
            .context("opencode reconciliation requires runtime")?;
        if self.opencode_driver()?.present(&record, runtime)? {
            return Ok(());
        }
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        ensure_session_not_reparent_fenced(&state, id)?;
        if session_runtime_launch_records(&state)?
            .iter()
            .any(|launch| launch.session_id == id && opencode_launch_pending(&launch.status))
        {
            return Ok(());
        }
        let Some(session) = session_object_mut(ensure_sessions_array_mut(&mut state)?, id) else {
            return Ok(());
        };
        if session
            .get("opencode_pending_retire")
            .is_some_and(|v| !v.is_null())
        {
            return Ok(());
        }
        if session
            .get("session_credential_sha256")
            .and_then(Value::as_str)
            != record.session_credential_sha256.as_deref()
            || serde_json::from_value::<Option<RuntimeBinding>>(
                session.get("opencode").cloned().unwrap_or(Value::Null),
            )? != record.opencode
        {
            return Ok(());
        }
        session.insert("status".into(), json!("stopped"));
        session.insert("stopped_at".into(), json!(now_rfc3339()));
        session.insert(
            "error_message".into(),
            json!(format!(
                "Opencode server exited; use sm restore to resume. Inspect {}/serve.log",
                record
                    .opencode
                    .as_ref()
                    .context("missing opencode binding")?
                    .state_dir
            )),
        );
        self.write_raw_json_value(&state)
    }

    pub(super) fn recover_opencode_runtime_launch(
        &self,
        pending: &SessionRuntimeLaunchRecord,
    ) -> Result<()> {
        let launch = pending;
        let _launch_guard = self.lock_clear_operation(&launch.session_id)?;
        let Some(launch) = session_runtime_launch_records(&self.load_parsed_state()?.raw)?
            .into_iter()
            .find(|launch| launch.id == pending.id && opencode_launch_pending(&launch.status))
        else {
            return Ok(());
        };
        let driver = self.opencode_driver()?;
        let runtime = self
            .delivery_runtime
            .as_ref()
            .context("opencode recovery requires runtime")?
            .for_socket_name(launch.tmux_socket_name.as_deref());
        let Some(record) = self.get_session(&launch.session_id)? else {
            let _guard = self.write_guard()?;
            let mut state = self.load_raw_json_value()?;
            mark_runtime_launch_failed(
                &mut state,
                &launch.id,
                &launch.session_id,
                false,
                "opencode provisional session missing",
            )?;
            return self.write_raw_json_value(&state);
        };
        if raw_session_object(&self.load_parsed_state()?.raw, &record.id).is_some_and(|r| {
            r.get("opencode_pending_retire")
                .is_some_and(|v| !v.is_null())
        }) {
            return Ok(());
        }
        if launch.status == "teardown_pending" {
            let stopped = driver.stop(&record, &runtime);
            self.record_opencode_teardown_result(
                &launch,
                &record,
                &stopped,
                launch
                    .failure_reason
                    .as_deref()
                    .unwrap_or("opencode launch failed"),
            )?;
            return stopped;
        }
        let preflight = (|| {
            if launch.provider_resume_id.is_none()
                || record.provider_resume_id != launch.provider_resume_id
            {
                anyhow::bail!("opencode launch has no committed conversation")
            }
            if record.session_credential_sha256.as_deref() != Some(&launch.credential_sha256)
                || completion_status_is_retired(record.completion_status.as_deref())
            {
                anyhow::bail!("opencode launch lost lifecycle authority")
            }
            Ok(record.clone())
        })();
        if let Err(error) = preflight {
            return self
                .handle_opencode_launch_result(
                    &launch,
                    &record,
                    &runtime,
                    driver.as_ref(),
                    Err(error),
                )
                .map(|_| ());
        }
        // Transport errors cannot prove absence and must preserve the launch.
        if !driver.present(&record, &runtime)? {
            return self
                .handle_opencode_launch_result(
                    &launch,
                    &record,
                    &runtime,
                    driver.as_ref(),
                    Err(anyhow::anyhow!(
                        "opencode launch runtime is absent; use explicit restore"
                    )),
                )
                .map(|_| ());
        }
        // HTTP uncertainty retains a launching record and exits this recovery
        // pass; it never replays a new identity or spins on the same launch.
        self.start_opencode_reader(&record.id, driver.as_ref())?;
        driver.attach(&record, &runtime)?;
        self.finish_opencode_launch(&launch.id, &runtime, driver.as_ref())
            .map(|_| ())
    }
}

fn opencode_launch_pending(status: &str) -> bool {
    matches!(status, "prepared" | "launching" | "teardown_pending")
}

fn opencode_terminal_metadata(state: &Value, id: &str) -> Result<Value> {
    let session = raw_session_object(state, id).context("restore session disappeared")?;
    Ok(Value::Object(
        [
            "completion_status",
            "completion_message",
            "completed_at",
            "stopped_at",
            "terminal_provenance",
            "retirement_intent",
            "agent_task_completed_at",
        ]
        .into_iter()
        .map(|field| {
            (
                field.into(),
                session.get(field).cloned().unwrap_or(Value::Null),
            )
        })
        .collect(),
    ))
}

fn opencode_restore_authority(state: &Value, id: &str) -> Result<Value> {
    let session = raw_session_object(state, id).context("restore session disappeared")?;
    Ok(Value::Object(
        [
            "provider",
            "node",
            "parent_session_id",
            "tmux_session",
            "tmux_socket_name",
            "opencode",
            "provider_resume_id",
            "session_credential_sha256",
            "completion_status",
            "completion_message",
            "completed_at",
            "stopped_at",
            "terminal_provenance",
            "retirement_intent",
            "agent_task_completed_at",
        ]
        .into_iter()
        .map(|field| {
            (
                field.into(),
                session.get(field).cloned().unwrap_or(Value::Null),
            )
        })
        .collect(),
    ))
}

fn occupied_opencode_sessions(state: &Value, exclude: Option<&str>) -> Result<Vec<SessionRecord>> {
    let pending: BTreeSet<_> = session_runtime_launch_records(state)?
        .into_iter()
        .filter(|launch| launch.provider == "opencode" && opencode_launch_pending(&launch.status))
        .map(|launch| launch.session_id)
        .collect();
    Ok(snapshot_from_raw_value(state)?
        .sessions
        .into_iter()
        .filter(|record| {
            record.provider == "opencode"
                && Some(record.id.as_str()) != exclude
                && (!record.is_stopped()
                    || pending.contains(&record.id)
                    || raw_session_object(state, &record.id).is_some_and(|r| {
                        r.get("opencode_pending_retire")
                            .is_some_and(|v| !v.is_null())
                    }))
        })
        .collect())
}
fn check_opencode_capacity(
    config: &OpencodeConfig,
    occupied: &[SessionRecord],
    predecessor: Option<&str>,
) -> Result<()> {
    if let Some(id) = predecessor {
        if occupied
            .iter()
            .any(|record| record.id == id && !record.is_stopped())
        {
            return Ok(());
        }
        anyhow::bail!("local handoff predecessor does not hold a seat")
    }
    if occupied.len() >= config.max_agents {
        return Err(crate::opencode::launch::AdmissionError(format!(
            "no local seat free ({}/{} used by {})",
            occupied.len(),
            config.max_agents,
            occupied
                .iter()
                .map(|record| record.friendly_name.as_deref().unwrap_or(&record.name))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .into());
    }
    Ok(())
}
fn reserve_opencode_port(config: &OpencodeConfig, occupied: &[SessionRecord]) -> Result<u16> {
    for port in config.port_range[0]..=config.port_range[1] {
        if !occupied.iter().any(|record| {
            record
                .opencode
                .as_ref()
                .is_some_and(|binding| binding.port == port)
        }) && std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
        {
            return Ok(port);
        }
    }
    Err(crate::opencode::launch::AdmissionError("no local port free".into()).into())
}
fn opencode_launch_record(
    record: &SessionRecord,
    request: &CreateCoreSessionRequest,
    id: String,
    operation: &str,
) -> SessionRuntimeLaunchRecord {
    let now = now_rfc3339();
    SessionRuntimeLaunchRecord {
        id,
        operation_kind: operation.into(),
        session_id: record.id.clone(),
        tmux_session: record.tmux_session.clone(),
        tmux_socket_name: record.tmux_socket_name.clone(),
        working_dir: record.working_dir.clone(),
        log_file: record.log_file.clone().unwrap_or_default(),
        provider: "opencode".into(),
        provider_resume_id: record.provider_resume_id.clone(),
        brief_message_id: None,
        brief_part_id: None,
        credential_rotation_id: None,
        restore_authorized: operation == "restore",
        opencode_restore_terminal_metadata: None,
        initial_message: request.initial_message.clone(),
        model: record.model.clone(),
        reasoning_effort: None,
        spawn_launch_intent_id: request
            .spawn_brief
            .as_ref()
            .map(|brief| brief.intent_id.clone()),
        spawn_brief_sha256: request
            .spawn_brief
            .as_ref()
            .map(|brief| brief.sha256.clone()),
        force_initial_prompt_stdin: false,
        credential_sha256: record.session_credential_sha256.clone().unwrap_or_default(),
        status: "launching".into(),
        created_at: now.clone(),
        updated_at: now,
        failure_reason: None,
    }
}
