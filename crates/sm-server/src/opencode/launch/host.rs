//! Production host adapter; requests cannot choose wall settings or tool sources.
use super::*;
use crate::{config::AppConfig, sessions::OpencodeLaunchDriver};
use std::net::SocketAddr;

pub struct HostDriver {
    config: AppConfig,
    walls: std::sync::Arc<GenerationWalls>,
    upstream: SocketAddr,
}
impl HostDriver {
    pub fn new(config: AppConfig, walls: std::sync::Arc<GenerationWalls>, port: u16) -> Self {
        Self {
            config,
            walls,
            upstream: SocketAddr::from(([127, 0, 0, 1], port)),
        }
    }
    fn queue(&self) -> Result<PathBuf> {
        self.config
            .queue_runner_state_dir()
            .canonicalize()
            .context("opencode queue authority unavailable")
    }
    fn files(&self, record: &SessionRecord) -> Result<LaunchFiles> {
        LaunchFiles::reopen(
            &self.config.opencode,
            record
                .opencode
                .as_ref()
                .context("missing opencode runtime binding")?,
        )
    }
    fn spec(record: &SessionRecord) -> Result<TmuxSessionSpec> {
        Ok(TmuxSessionSpec {
            session_id: record.id.clone(),
            session_credential: None,
            tmux_session: record.tmux_session.clone(),
            working_dir: expand_home(&record.working_dir).display().to_string(),
            log_file: expand_home(
                record
                    .log_file
                    .as_deref()
                    .context("opencode log file missing")?,
            ),
            provider: "opencode".into(),
            initial_message: None,
            force_initial_prompt_stdin: false,
            claude_session_id: None,
            model: record.model.clone(),
            reasoning_effort: None,
        })
    }
}
impl OpencodeLaunchDriver for HostDriver {
    fn requires_reader(&self) -> bool {
        true
    }
    fn base_config(&self) -> OpencodeConfig {
        self.config.opencode.clone()
    }
    fn config(&self, requested: Option<&str>) -> Result<OpencodeConfig> {
        let host = crate::local_model::live(&self.queue()?).context("no local model loaded")?;
        let config = loaded_config(&self.config.opencode, host.record()?.as_ref(), requested)?;
        verify_version(&config)?;
        Ok(config)
    }
    fn binding(&self, config: &OpencodeConfig, id: &str, port: u16) -> Result<RuntimeBinding> {
        validate_component(id)?;
        let root = expand_home(&config.state_root);
        private_directory(&root)?;
        let root = root.canonicalize()?;
        Ok(RuntimeBinding {
            port,
            state_dir: root.join(id).display().to_string(),
            version: config.version.clone(),
            model_base_url: config.model_base_url.clone(),
        })
    }
    fn start(
        &self,
        config: &OpencodeConfig,
        record: &SessionRecord,
        credential: &str,
        runtime: &TmuxRuntime,
    ) -> Result<()> {
        let binding = record
            .opencode
            .as_ref()
            .context("missing opencode binding")?;
        let files = LaunchFiles::prepare(config, &record.id, binding.port)?;
        if files.binding != *binding {
            bail!("opencode prepared binding changed")
        }
        // Preparation can take seconds; refuse if a model unload/change began
        // while the provisional local seat was being prepared.
        let current = self.config(Some(&config.model_id))?;
        if current.model_base_url != config.model_base_url
            || current.context_window != config.context_window
        {
            bail!("loaded model changed during opencode launch")
        }
        let home = expand_home("~").canonicalize()?;
        let mut roots = vec![
            PathBuf::from("/opt/homebrew"),
            home.join(".rustup/toolchains"),
            home.join(".cargo/registry"),
            home.join(".cargo/git"),
        ];
        roots.retain(|root| root.is_dir());
        let installed = std::env::current_exe()?.canonicalize()?;
        let mut tools = vec![StageTool {
            name: "sm".into(),
            source: installed
                .parent()
                .context("installed CLI directory missing")?
                .join("sm")
                .canonicalize()?,
        }];
        for name in ["gh", "node", "cargo", "rustc"] {
            if let Some(source) = host_tool(name)? {
                tools.push(StageTool {
                    name: name.into(),
                    source,
                });
            }
        }
        let checkout = expand_home(&record.working_dir).canonicalize()?;
        let mut git = Command::new("/usr/bin/git");
        git.args([
            "-C",
            checkout.to_str().context("checkout is not UTF-8")?,
            "branch",
            "--show-current",
        ]);
        let branch = command_output_with_timeout(git, Duration::from_secs(5))?;
        if !branch.status.success() {
            bail!("opencode checkout is not a git repository")
        }
        let host = HostConfiguration {
            home,
            state_root: PathBuf::from(&binding.state_dir)
                .parent()
                .context("state root missing")?
                .to_path_buf(),
            alias_root: PathBuf::from(format!("/private/tmp/sm-opencode-{}", unsafe {
                libc::geteuid()
            })),
            python: expand_home(&self.config.local_judge.python),
            read_only_roots: roots.clone(),
            executable_roots: roots,
            control_ports: config.port_range[0]..=config.port_range[1],
            model_port: super::super::loopback_url(&config.model_base_url)?
                .port_u16()
                .context("model port missing")?,
            sm_upstream: self.upstream,
        };
        let agent = AgentRegistration {
            id: record.id.clone(),
            name: record
                .friendly_name
                .as_deref()
                .unwrap_or(&record.name)
                .into(),
            ticket: 0,
            title: record
                .current_task
                .as_deref()
                .unwrap_or("local agent")
                .into(),
            branch: String::from_utf8(branch.stdout)?.trim().into(),
            checkout,
            parent: record
                .parent_session_id
                .clone()
                .unwrap_or_else(|| "owner".into()),
            control_port: binding.port,
            tools,
        };
        let owner = files.stage(&self.walls, host, agent, &installed, credential)?;
        files.start_server(
            &runtime.for_socket_name(record.tmux_socket_name.as_deref()),
            &Self::spec(record)?,
            &owner,
        )
    }
    fn client(&self, binding: &RuntimeBinding) -> Result<Client> {
        LaunchFiles::reopen(&self.config.opencode, binding)?.client(Duration::from_secs(5))
    }
    fn attach(&self, record: &SessionRecord, runtime: &TmuxRuntime) -> Result<()> {
        self.files(record)?.attach(
            &runtime.for_socket_name(record.tmux_socket_name.as_deref()),
            &Self::spec(record)?,
            record
                .provider_resume_id
                .as_deref()
                .context("opencode conversation missing")?,
        )
    }
    fn present(&self, record: &SessionRecord, runtime: &TmuxRuntime) -> Result<bool> {
        let runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        match runtime.probe_session_for_restore(&record.tmux_session) {
            crate::runtime::RestoreTmuxLivenessOutcome::Live => {
                runtime.opencode_serve_alive(&record.tmux_session)
            }
            crate::runtime::RestoreTmuxLivenessOutcome::Absent => Ok(false),
            crate::runtime::RestoreTmuxLivenessOutcome::Inconclusive { reason } => {
                bail!("opencode tmux liveness unresolved: {reason}")
            }
        }
    }
    fn stop(&self, record: &SessionRecord, runtime: &TmuxRuntime) -> Result<()> {
        let queue = self.queue()?;
        // Request clean owner retirement first, then stop the host restart
        // launcher. An unreachable owner still needs receipt/reboot proof.
        if let Some(owner) = crate::local_wall::owner::OwnerClient::registered(&queue, &record.id)?
        {
            let _ = owner.retire();
        }
        let runtime = runtime.for_socket_name(record.tmux_socket_name.as_deref());
        runtime.ensure_recovery_server_anchor()?;
        match runtime.probe_session_for_restore(&record.tmux_session) {
            crate::runtime::RestoreTmuxLivenessOutcome::Live => {
                runtime.kill_session(&record.tmux_session)?;
            }
            crate::runtime::RestoreTmuxLivenessOutcome::Absent => {}
            crate::runtime::RestoreTmuxLivenessOutcome::Inconclusive { reason } => {
                bail!("opencode teardown unresolved: {reason}")
            }
        }
        if !matches!(
            runtime.probe_session_for_restore(&record.tmux_session),
            crate::runtime::RestoreTmuxLivenessOutcome::Absent
        ) {
            bail!("opencode tmux launcher is not confirmed stopped")
        }
        crate::queue::local_wall::cancel_pending_jobs_for_restaging(&queue, &record.id)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match crate::local_wall::owner::retire_for_restaging(&queue, &record.id) {
                Ok(()) => return Ok(()),
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}
fn host_tool(name: &str) -> Result<Option<PathBuf>> {
    if matches!(name, "cargo" | "rustc") {
        let mut command = Command::new("rustup");
        command.args(["which", name]);
        if let Ok(output) = command_output_with_timeout(command, Duration::from_secs(5)) {
            if output.status.success() {
                return Ok(Some(
                    PathBuf::from(String::from_utf8(output.stdout)?.trim()).canonicalize()?,
                ));
            }
        }
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|path| path.join(name))
        .find(|path| path.is_file())
        .map(|path| path.canonicalize())
        .transpose()
        .map_err(Into::into)
}
