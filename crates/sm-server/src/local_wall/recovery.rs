//! Server-generation ownership of saved production wall authority.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Deserialize)]
struct SavedTool {
    name: String,
    source: PathBuf,
}
#[derive(Deserialize)]
struct SavedAgent {
    id: String,
    name: String,
    ticket: i64,
    title: String,
    branch: String,
    checkout: PathBuf,
    parent: String,
    control_port: u16,
    tools: Vec<SavedTool>,
}
#[derive(Deserialize)]
struct SavedAuthority {
    home: PathBuf,
    state_root: PathBuf,
    endpoint: PathBuf,
    read_only_roots: Vec<PathBuf>,
    executable_roots: Vec<PathBuf>,
    ranges: BTreeMap<String, String>,
    registration: SavedAgent,
}
struct OwnedWalls {
    runtime: Option<LocalWallRuntime>,
    walls: BTreeMap<String, Arc<PreparedWall>>,
    durable: BTreeMap<String, owner::OwnerClient>,
}

/// Host-only service shared by startup, queue recovery and future providers.
/// Saved JSON is read only from host-owned, validated registration locations.
pub struct GenerationWalls {
    queue_state: PathBuf,
    python: PathBuf,
    upstream: SocketAddr,
    model_port: u16,
    egress: ServiceClient,
    judge: LocalJudgeRuntime,
    stopped: AtomicBool,
    owned: Mutex<OwnedWalls>,
}
impl GenerationWalls {
    pub fn new(
        queue_state: PathBuf,
        python: PathBuf,
        upstream: SocketAddr,
        model_url: &str,
        egress: ServiceClient,
        judge: LocalJudgeRuntime,
    ) -> Result<Self> {
        if !upstream.ip().is_loopback() || upstream.port() == 0 {
            bail!("sm upstream must be loopback");
        }
        Ok(Self {
            queue_state,
            python,
            upstream,
            model_port: loopback_port(model_url)?,
            egress,
            judge,
            stopped: AtomicBool::new(false),
            owned: Mutex::new(OwnedWalls {
                runtime: None,
                walls: BTreeMap::new(),
                durable: BTreeMap::new(),
            }),
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<Arc<PreparedWall>>> {
        if self.stopped.load(Ordering::Acquire) {
            bail!("wall generation stopped");
        }
        Ok(self
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("wall generation lock poisoned"))?
            .walls
            .get(id)
            .cloned())
    }

    /// Providers pass host configuration; submitted jobs cannot call this API.
    pub fn prepare(
        &self,
        configuration: HostConfiguration,
        agent: &AgentRegistration,
    ) -> Result<Arc<PreparedWall>> {
        if owner::OwnerClient::registered(&self.queue_state, &agent.id)?.is_some() {
            bail!("durable provider owns this wall; reconnect to its host launcher");
        }
        let mut owned = self
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("wall generation lock poisoned"))?;
        if self.stopped.load(Ordering::Acquire) {
            bail!("wall generation stopped");
        }
        if owned.walls.contains_key(&agent.id) {
            bail!("agent wall already retained; use get");
        }
        if let Some(runtime) = &owned.runtime {
            if runtime.config != configuration {
                bail!("agents in one generation must share host wall configuration");
            }
        } else {
            if configuration.sm_upstream != self.upstream
                || configuration.model_port != self.model_port
            {
                bail!("wall configuration differs from server endpoints");
            }
            owned.runtime = Some(LocalWallRuntime::new(
                configuration,
                self.egress.clone(),
                self.judge.clone(),
            )?);
        }
        let wall = owned
            .runtime
            .as_ref()
            .unwrap()
            .prepare_for_queue(agent, &self.queue_state)?;
        owned.walls.insert(agent.id.clone(), wall.clone());
        Ok(wall)
    }

    /// Stage the exact command for a host tmux window. Starting that command
    /// prepares the wall and retains it independently of this server generation.
    pub fn stage_provider(
        &self,
        configuration: HostConfiguration,
        agent: AgentRegistration,
        provider: owner::ProviderLaunch,
        installed_executable: &Path,
    ) -> Result<owner::HostLaunch> {
        let _admission = crate::queue::admission_guard();
        let owned = self
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("wall generation lock poisoned"))?;
        if self.stopped.load(Ordering::Acquire) || owned.walls.contains_key(&agent.id) {
            bail!("generation stopped or agent already has generation-owned authority");
        }
        if configuration.sm_upstream != self.upstream || configuration.model_port != self.model_port
        {
            bail!("wall configuration differs from server endpoints");
        }
        owner::stage(
            &self.queue_state,
            configuration,
            agent,
            self.egress.clone(),
            self.judge.clone(),
            provider,
            installed_executable,
        )
    }

    pub fn get_durable(&self, id: &str) -> Result<Option<owner::OwnerClient>> {
        if self.stopped.load(Ordering::Acquire) {
            bail!("wall generation stopped");
        }
        Ok(self
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("wall generation lock poisoned"))?
            .durable
            .get(id)
            .cloned())
    }

    /// An unavailable predecessor or incomplete authority is reported per agent.
    /// Reconcile again after old launches/locks finish; no host fallback occurs.
    pub fn reconcile(&self) -> Result<Vec<String>> {
        if self.stopped.load(Ordering::Acquire) {
            return Ok(Vec::new());
        }
        let directory = self.queue_state.join("local-walls");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        physical_directory(&directory)?;
        let mut failures = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            if self.get(id)?.is_some() {
                continue;
            }
            let result = (|| {
                if let Some(owner) = owner::OwnerClient::registered(&self.queue_state, id)? {
                    let mut owned = self
                        .owned
                        .lock()
                        .map_err(|_| anyhow::anyhow!("wall generation lock poisoned"))?;
                    if self.stopped.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    crate::queue::local_wall::attach_durable(&self.queue_state, id, owner.clone())?;
                    owned.durable.insert(id.into(), owner);
                    return Ok(());
                }
                let spec = crate::queue::local_wall::registered_spec(&self.queue_state, id)?
                    .context("missing saved wall")?;
                if spec.host_authority_sha256.is_none() {
                    bail!("registration lacks pinned production host authority");
                }
                let authority = spec.agent_state.join("xdg/config/queue-authority.json");
                if authority.canonicalize()? != authority {
                    bail!("saved authority has path aliases");
                }
                let mut file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&authority)?;
                let metadata = file.metadata()?;
                if !metadata.is_file()
                    || metadata.nlink() != 1
                    || metadata.uid() != unsafe { libc::geteuid() }
                {
                    bail!("saved authority is not an independent host file");
                }
                use std::io::Read;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                let saved: SavedAuthority = serde_json::from_slice(&bytes)?;
                if saved.registration.id != id || saved.state_root.join(id) != spec.agent_state {
                    bail!("saved agent identity/state changed");
                }
                let range = saved.ranges.get("agent").context("missing control range")?;
                let (first, last) = range.split_once('-').context("invalid control range")?;
                let alias_root = saved
                    .endpoint
                    .parent()
                    .and_then(Path::parent)
                    .context("invalid broker alias")?
                    .to_path_buf();
                let configuration = HostConfiguration {
                    home: saved.home,
                    state_root: saved.state_root,
                    alias_root,
                    python: self.python.clone(),
                    read_only_roots: saved.read_only_roots,
                    executable_roots: saved.executable_roots,
                    control_ports: first.parse()?..=last.parse()?,
                    model_port: self.model_port,
                    sm_upstream: self.upstream,
                };
                let saved = saved.registration;
                let agent = AgentRegistration {
                    id: saved.id,
                    name: saved.name,
                    ticket: saved.ticket,
                    title: saved.title,
                    branch: saved.branch,
                    checkout: saved.checkout,
                    parent: saved.parent,
                    control_port: saved.control_port,
                    tools: saved
                        .tools
                        .into_iter()
                        .map(|tool| StageTool {
                            name: tool.name,
                            source: tool.source,
                        })
                        .collect(),
                };
                self.prepare(configuration, &agent).map(|_| ())
            })();
            if let Err(error) = result {
                failures.push(format!("{id}: {error:#}"));
            }
        }
        Ok(failures)
    }

    /// Stops new preparation and admission. Running children keep their own
    /// service reference until supervisor cleanup; durable registrations remain.
    pub fn stop(&self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        let _admission = crate::queue::admission_guard();
        let mut owned = self
            .owned
            .lock()
            .map_err(|_| anyhow::anyhow!("wall generation lock poisoned"))?;
        for (id, wall) in &owned.walls {
            wall.stop_admission()?;
            crate::queue::local_wall::detach(&self.queue_state, id)?;
        }
        owned.walls.clear();
        for id in owned.durable.keys() {
            crate::queue::local_wall::detach(&self.queue_state, id)?;
        }
        owned.durable.clear();
        owned.runtime = None;
        Ok(())
    }
    pub fn guard(self: &Arc<Self>) -> GenerationGuard {
        GenerationGuard(self.clone())
    }
}
pub struct GenerationGuard(Arc<GenerationWalls>);
impl Drop for GenerationGuard {
    fn drop(&mut self) {
        let _ = self.0.stop();
    }
}
impl Drop for GenerationWalls {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::RetainedQueueStore;
    use std::time::{Duration, Instant};

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "spawned by the production wall integration test with private fixture paths"]
    async fn restore_child() {
        let input: serde_json::Value = serde_json::from_slice(
            &fs::read(std::env::var_os("SM_WALL_RESTORE_FIXTURE").unwrap()).unwrap(),
        )
        .unwrap();
        let path = |key: &str| PathBuf::from(input[key].as_str().unwrap());
        let queue = path("queue");
        let mut config = crate::config::AppConfig::default();
        config.local_judge.port = input["judge_port"].as_u64().unwrap() as u16;
        config.local_host.base_url = input["model_url"].as_str().unwrap().into();
        let walls = Arc::new(
            GenerationWalls::new(
                queue.clone(),
                path("python"),
                input["upstream"].as_str().unwrap().parse().unwrap(),
                &config.local_host.base_url,
                ServiceClient::new(path("egress"), std::env::current_exe().unwrap()),
                LocalJudgeRuntime::isolated(&config, path("judge")),
            )
            .unwrap(),
        );
        let failures = walls.reconcile().unwrap();
        assert!(failures.is_empty(), "{failures:?}");
        let wall = walls.get("wall-c").unwrap().unwrap();
        let old_peer: [u32; 8] = serde_json::from_value(input["old_peer"].clone()).unwrap();
        assert_ne!(wall.broker_peer_token().0, old_peer);
        assert!(walls.get("wall-d").unwrap().is_some());
        let job = input["job"].as_str().unwrap();
        RetainedQueueStore::start_queue_job_in_state_dir(
            &queue,
            &queue.join("messages.db"),
            job,
            0,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let result = RetainedQueueStore::get_queue_job_strict_from_path(
                &queue.join("queue_runner.db"),
                job,
            )
            .unwrap()
            .unwrap();
            if result.state != "pending" && result.state != "running" {
                let log = fs::read_to_string(result.log_path.unwrap()).unwrap();
                assert_eq!(result.state, "succeeded", "{log}");
                assert!(log.contains("durable-wall-ok"), "{log}");
                break;
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pending = RetainedQueueStore::create_queue_job_in_state_dir(
            &queue,
            crate::queue::CreateQueueJob {
                local_submitter: Some(
                    crate::local_egress::gateway::VerifiedLocalAgent::test_identity("wall-c"),
                ),
                job_type: "tests".into(),
                label: "generation-shutdown".into(),
                requester_session_id: None,
                notify_session_id: "wall-c".into(),
                cwd: wall.checkout.display().to_string(),
                argv: None,
                script: Some("print must-not-launch-after-stop".into()),
                env: BTreeMap::new(),
                timeout_seconds: 10,
                cpu_percent: None,
                gpu_percent: None,
                memory_bytes: None,
                rank_tickets: None,
            },
        )
        .unwrap();
        // Represent an admission pass still publishing its running row. Stop
        // must wait for that complete critical section before detaching.
        let admission = crate::queue::admission_guard();
        let stopping = walls.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let stopping_thread = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            let result = stopping.stop();
            done_tx.send(result).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(admission);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        stopping_thread.join().unwrap();
        let held = RetainedQueueStore::start_queue_job_in_state_dir(
            &queue,
            &queue.join("messages.db"),
            &pending.id,
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(held.state, "pending");
        assert_eq!(held.holding_reason.as_deref(), Some("local_wall"));
        assert!(walls.get("wall-c").is_err());
        assert!(wall.spawn_provider("probe", &["provider".into()]).is_err());
        assert!(wall.spawn_queue("probe", &["hold".into()]).is_err());
        let cancelled = RetainedQueueStore::cancel_queue_job_in_state_dir(
            &queue,
            &queue.join("messages.db"),
            &pending.id,
            0,
            crate::queue::QueueAdmissionPolicy::default(),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(cancelled.state, "cancelled");
        drop(wall);
    }
}
