//! Host-only launch driver. The session store owns admission serialization,
//! durable launch/conversation/brief commits and event-reader startup.
use super::{Client, DeliveryOutcome, MessageBinding, OpencodeConfig, RuntimeBinding};
pub mod host;
use crate::{
    local_model::ModelRecord,
    local_wall::{
        owner::{OwnerClient, ProviderLaunch},
        recovery::GenerationWalls,
        AgentRegistration, HostConfiguration, StageTool,
    },
    runtime::{
        command_output_with_timeout, InitialBriefDeliveryError, TmuxRuntime, TmuxSessionSpec,
    },
    sessions::{expand_home, SessionRecord},
};
use anyhow::{bail, Context, Result};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

/// A launch refusal whose prerequisites can be corrected before retrying.
#[derive(Debug)]
pub struct AdmissionError(pub String);
impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AdmissionError {}

/// Include live seats and provisional launches. Hold the store's admission
/// lock through reservation persistence; a stopped provisional record alone
/// does not reserve capacity or a port. Only server-authorized handoff supplies
/// a predecessor, never a field from an untrusted creation request.
pub fn check_seat(
    config: &OpencodeConfig,
    occupied: &[SessionRecord],
    predecessor: Option<&str>,
) -> Result<()> {
    let seats: Vec<_> = occupied
        .iter()
        .filter(|s| s.provider == "opencode")
        .collect();
    if let Some(id) = predecessor {
        if seats.iter().any(|seat| seat.id == id) {
            return Ok(());
        }
        bail!("local handoff predecessor does not hold a seat");
    }
    if seats.len() >= config.max_agents {
        let names = seats
            .iter()
            .map(|seat| seat.friendly_name.as_deref().unwrap_or(&seat.name))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(AdmissionError(format!(
            "no local seat free ({}/{} used by {names})",
            seats.len(),
            config.max_agents
        ))
        .into());
    }
    Ok(())
}

/// #1957 is shipped: the durable model record, rather than an arbitrary HTTP
/// response, authorizes launch. Render the loaded identifier and context.
pub fn loaded_config(
    config: &OpencodeConfig,
    model: Option<&ModelRecord>,
    requested: Option<&str>,
) -> Result<OpencodeConfig> {
    let model = model
        .filter(|m| m.state == "ready")
        .ok_or_else(|| AdmissionError("no local model loaded".into()))?;
    if let Some(requested) = requested.map(str::trim).filter(|m| !m.is_empty()) {
        if requested != model.identifier {
            return Err(AdmissionError(format!(
                "model {requested} is not loaded; loaded: {}",
                model.identifier
            ))
            .into());
        }
    }
    let mut effective = config.clone();
    effective.model_id = model.identifier.clone();
    effective.context_window = model.context;
    let endpoint = model.endpoint.trim_end_matches('/');
    effective.model_base_url = if endpoint.ends_with("/v1") {
        endpoint.into()
    } else {
        format!("{endpoint}/v1")
    };
    effective.validate()?;
    Ok(effective)
}

pub fn lowest_port(config: &OpencodeConfig, occupied: &BTreeSet<u16>) -> Result<u16> {
    config.validate()?;
    for port in config.port_range[0]..=config.port_range[1] {
        if !occupied.contains(&port) && TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return Ok(port);
        }
    }
    Err(AdmissionError("no local port free".into()).into())
}

pub fn verify_version(config: &OpencodeConfig) -> Result<()> {
    let mut command = Command::new(expand_home(&config.binary));
    command.arg("--version");
    let output = command_output_with_timeout(command, Duration::from_secs(5))?;
    let found = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || found != config.version {
        return Err(AdmissionError(format!(
            "opencode {found} is installed; sm is pinned to {}",
            config.version
        ))
        .into());
    }
    Ok(())
}

/// Private state files remain after failed launches. This object deliberately
/// has no Debug implementation, because it holds the server password.
pub struct LaunchFiles {
    pub binding: RuntimeBinding,
    pub serve_script: PathBuf,
    pub attach_script: PathBuf,
    pub serve_log: PathBuf,
    config: OpencodeConfig,
    password: String,
}

impl LaunchFiles {
    /// Called after admission/model checks, before starting the wall owner.
    /// Restoration must first prove the previous provider has exited.
    pub fn prepare(config: &OpencodeConfig, id: &str, port: u16) -> Result<Self> {
        config.validate()?;
        validate_component(id)?;
        if !(config.port_range[0]..=config.port_range[1]).contains(&port) {
            bail!("opencode port is outside the reserved control range");
        }
        verify_version(config)?;
        let root = expand_home(&config.state_root);
        private_directory(&root)?;
        let root = root.canonicalize()?;
        let sdk = prepare_sdk(config, &root)?;
        let state = root.join(id);
        private_directory(&state)?;
        let result: Result<Self> = (|| {
            for path in [
                "xdg/config/opencode/plugins",
                "xdg/data",
                "xdg/cache",
                "xdg/state",
                "tmp",
            ] {
                private_directory(&state.join(path))?;
            }
            let folder = state.join("xdg/config/opencode");
            let mut agent_config: serde_json::Value =
                serde_json::from_str(&config.render_agent_config()?)?;
            // Platform shells discard the injected adapter. Use the immutable
            // ad-hoc signed shell already staged by the production wall.
            agent_config["shell"] =
                serde_json::json!(state.join("xdg/config/executables/queue-zsh"));
            host_write(
                &folder.join("opencode.json"),
                serde_json::to_string_pretty(&agent_config)?.as_bytes(),
                0o600,
            )?;
            // Config initialization creates this file even with no plugins.
            // Pre-stage it before the wall makes configuration write-denied.
            host_write(
                &folder.join(".gitignore"),
                b"node_modules\npackage.json\npackage-lock.json\nbun.lock\n.gitignore\n",
                0o600,
            )?;
            host_write(
                &folder.join("plugins/sm_judge.js"),
                include_bytes!("../../../../scripts/opencode/sm_judge.js"),
                0o600,
            )?;
            copy_sdk(&sdk, &folder)?;
            let password = password(&state.join("server.secret"))?;
            Ok(Self {
                binding: RuntimeBinding {
                    port,
                    state_dir: state.display().to_string(),
                    version: config.version.clone(),
                    model_base_url: config.model_base_url.clone(),
                },
                serve_script: state.join("launch-serve.sh"),
                attach_script: state.join("launch-attach.sh"),
                serve_log: state.join("serve.log"),
                config: config.clone(),
                password,
            })
        })();
        result.with_context(|| format!("opencode launch state retained at {}", state.display()))
    }

    /// Reuse immutable launch files during crash recovery; never prepare or
    /// rewrite a live wall's configuration merely to reconnect or attach.
    pub fn reopen(config: &OpencodeConfig, binding: &RuntimeBinding) -> Result<Self> {
        config.validate()?;
        binding.validate()?;
        let root = expand_home(&config.state_root).canonicalize()?;
        let state = Path::new(&binding.state_dir);
        if state.parent() != Some(root.as_path()) || state.canonicalize()? != state {
            bail!("opencode launch state is outside its physical root")
        }
        let secret = read_secret(&state.join("server.secret"))?;
        let native: serde_json::Value = serde_json::from_reader(private_file(
            &state.join("xdg/config/opencode/opencode.json"),
        )?)?;
        let model = native["model"]
            .as_str()
            .and_then(|model| model.strip_prefix("local/"))
            .context("opencode persisted model missing")?;
        let limits = &native["provider"]["local"]["models"][model]["limit"];
        let effective = OpencodeConfig {
            model_id: model.into(),
            model_base_url: binding.model_base_url.clone(),
            context_window: limits["context"]
                .as_u64()
                .context("persisted context limit missing")?,
            output_limit: limits["output"]
                .as_u64()
                .context("persisted output limit missing")?,
            ..config.clone()
        };
        effective.validate()?;
        Ok(Self {
            binding: binding.clone(),
            serve_script: state.join("launch-serve.sh"),
            attach_script: state.join("launch-attach.sh"),
            serve_log: state.join("serve.log"),
            config: effective,
            password: secret,
        })
    }

    pub fn client(&self, timeout: Duration) -> Result<Client> {
        Client::new(self.binding.port, &self.password, timeout)
    }

    /// Never start opencode directly. The staged owner prepares and owns the
    /// production wall independently of an sm server generation.
    pub fn stage(
        &self,
        walls: &GenerationWalls,
        host: HostConfiguration,
        mut agent: AgentRegistration,
        installed: &Path,
        credential: &str,
    ) -> Result<OwnerClient> {
        if host.state_root.join(&agent.id) != Path::new(&self.binding.state_dir)
            || host.control_ports != (self.config.port_range[0]..=self.config.port_range[1])
            || agent.control_port != self.binding.port
        {
            bail!("opencode launch and wall binding disagree");
        }
        if agent.tools.iter().any(|tool| tool.name == "opencode") {
            bail!("opencode launch owns the provider tool registration");
        }
        let contents = fs::read(expand_home(&self.config.binary))?;
        let directory = Path::new(&self.binding.state_dir)
            .parent()
            .context("opencode state has no parent")?
            .join("provider-binaries");
        private_directory(&directory)?;
        let source = directory.join(format!("opencode-{:x}", Sha256::digest(&contents)));
        if !source.try_exists()? {
            host_write(&source, &contents, 0o500)?;
        }
        private_file(&source)?;
        if fs::read(&source)? != contents {
            bail!("cached opencode executable changed");
        }
        agent.tools.push(StageTool {
            name: "opencode".into(),
            source,
        });
        let launch = walls.stage_provider(
            host,
            agent,
            ProviderLaunch {
                tool: "opencode".into(),
                arguments: [
                    "serve",
                    "--port",
                    &self.binding.port.to_string(),
                    "--hostname",
                    "127.0.0.1",
                    "--print-logs",
                    "--log-level",
                    "INFO",
                ]
                .into_iter()
                .map(OsString::from)
                .collect(),
                settings: BTreeMap::from([
                    ("OPENCODE_DISABLE_AUTOUPDATE".into(), "1".into()),
                    ("OPENCODE_DISABLE_MODELS_FETCH".into(), "1".into()),
                    ("OPENCODE_DISABLE_LSP_DOWNLOAD".into(), "1".into()),
                    ("OPENCODE_ENABLE_EXA".into(), "1".into()),
                    ("OPENCODE_SERVER_USERNAME".into(), "opencode".into()),
                    ("OPENCODE_SERVER_PASSWORD".into(), self.password.clone()),
                    ("SM_SESSION_CREDENTIAL".into(), credential.into()),
                    (
                        "SM_JUDGE_PLUGIN_LOG".into(),
                        format!("{}/xdg/state/plugin.jsonl", self.binding.state_dir),
                    ),
                ]),
            },
            installed,
        )?;
        host_write(
            &self.serve_script,
            serve_script(
                &launch.executable,
                &launch.arguments,
                Path::new(&self.binding.state_dir),
            )?
            .as_bytes(),
            0o700,
        )?;
        Ok(launch.client)
    }

    pub fn start_server(
        &self,
        runtime: &TmuxRuntime,
        spec: &TmuxSessionSpec,
        owner: &OwnerClient,
    ) -> Result<()> {
        runtime.create_opencode_serve_window(spec, &self.serve_script, &self.serve_log)?;
        let deadline = Instant::now() + Duration::from_secs(self.config.health_timeout_secs);
        let readiness: Result<()> = (|| {
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                if self
                    .client(remaining.min(Duration::from_secs(1)))?
                    .ready()
                    .unwrap_or(false)
                {
                    return Ok(());
                }
                if !runtime.session_exists(&spec.tmux_session)? {
                    break;
                }
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(250)),
                );
            }
            bail!("authenticated readiness timed out or tmux exited")
        })();
        if readiness.is_ok() {
            return Ok(());
        }
        let retirement = owner.retire();
        let _ = runtime.kill_session(&spec.tmux_session);
        let log = fs::read_to_string(&self.serve_log).unwrap_or_default();
        let tail = log
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let cleanup = retirement
            .err()
            .map(|e| format!("; wall retirement: {e:#}"))
            .unwrap_or_default();
        bail!(
            "opencode server did not start: {}; {tail}; state {}{cleanup}",
            readiness.unwrap_err(),
            self.binding.state_dir
        )
    }

    /// The caller persists the newly created conversation and starts its event
    /// reader before opening this view. The view gets no wall or sm credentials.
    pub fn attach(
        &self,
        runtime: &TmuxRuntime,
        spec: &TmuxSessionSpec,
        conversation: &str,
    ) -> Result<()> {
        super::validate_id(conversation, "ses")?;
        host_write(
            &self.attach_script,
            attach_script(&self.config, &self.binding, conversation).as_bytes(),
            0o700,
        )?;
        runtime.create_opencode_attach_window(spec, &self.attach_script)
    }

    pub fn replace_attach(
        &self,
        runtime: &TmuxRuntime,
        spec: &TmuxSessionSpec,
        conversation: &str,
    ) -> Result<()> {
        super::validate_id(conversation, "ses")?;
        host_write(
            &self.attach_script,
            attach_script(&self.config, &self.binding, conversation).as_bytes(),
            0o700,
        )?;
        runtime.replace_opencode_attach_window(spec, &self.attach_script)
    }

    /// Binding must already be committed in the launch record. Every retry GETs
    /// it before POSTing, so accepted briefs are not appended a second time.
    pub fn deliver_brief(
        &self,
        binding: &MessageBinding,
        text: &str,
        timeout: Duration,
    ) -> Result<()> {
        binding.validate()?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let request_budget = (remaining / 4).min(Duration::from_secs(1));
            if request_budget.is_zero() {
                break;
            }
            let client = self.client(request_budget)?;
            if matches!(
                client.attempt_delivery(
                    binding,
                    text,
                    (remaining / 2).min(Duration::from_secs(self.config.confirm_timeout_secs))
                ),
                Ok(DeliveryOutcome::Accepted)
            ) {
                return Ok(());
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(1)),
            );
        }
        Err(InitialBriefDeliveryError::ProviderAcceptanceTimedOut {
            provider: "opencode".into(),
        }
        .into())
    }
}

fn validate_component(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        bail!("invalid opencode state identity");
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    if path.canonicalize()? != path {
        bail!(
            "opencode state directory has path aliases: {}",
            path.display()
        );
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("opencode state directory is not host owned");
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!(
            "opencode host file is not independent and private: {}",
            path.display()
        );
    }
    Ok(file)
}

fn host_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if path.try_exists()? || path.is_symlink() {
        private_file(path)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(mode)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        bail!("opencode host file has aliases");
    }
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn password(path: &Path) -> Result<String> {
    if !path.try_exists()? && !path.is_symlink() {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let secret = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(secret.as_bytes())?;
        file.sync_all()?;
    }
    read_secret(path)
}

fn read_secret(path: &Path) -> Result<String> {
    let mut secret = String::new();
    private_file(path)?.take(65).read_to_string(&mut secret)?;
    if secret.len() != 64 || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid opencode server secret");
    }
    Ok(secret)
}

fn prepare_sdk(config: &OpencodeConfig, root: &Path) -> Result<PathBuf> {
    if config.version.is_empty()
        || !config
            .version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        || matches!(config.version.as_str(), "." | "..")
    {
        bail!("invalid opencode SDK version directory");
    }
    let sdk = root.join("plugin-sdk").join(&config.version);
    private_directory(&sdk)?;
    let lock_path = sdk.join("prepare.lock");
    host_write(&lock_path, b"", 0o600)?;
    let lock = private_file(&lock_path)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let folder = sdk.join("config/opencode");
    if folder
        .join("node_modules/@opencode-ai/plugin/package.json")
        .is_file()
    {
        return Ok(folder);
    }
    for path in ["config/opencode/plugins", "data", "cache", "state", "home"] {
        private_directory(&sdk.join(path))?;
    }
    host_write(
        &folder.join("opencode.json"),
        b"{\"autoupdate\":false,\"share\":\"disabled\",\"snapshot\":false,\"plugin\":[]}\n",
        0o600,
    )?;
    // The pinned runtime waits for its background dependency installation only
    // when it discovers an external plugin. Prepare with the same judge plugin
    // that the agent will load, so debug config cannot exit before npm finishes.
    host_write(
        &folder.join("plugins/sm_judge.js"),
        include_bytes!("../../../../scripts/opencode/sm_judge.js"),
        0o600,
    )?;
    let mut command = Command::new(expand_home(&config.binary));
    command
        .env_clear()
        .current_dir(&sdk)
        .args(["debug", "config"])
        .env("HOME", sdk.join("home"))
        .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
        .env("XDG_CONFIG_HOME", sdk.join("config"))
        .env("XDG_DATA_HOME", sdk.join("data"))
        .env("XDG_CACHE_HOME", sdk.join("cache"))
        .env("XDG_STATE_HOME", sdk.join("state"))
        .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
        .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "1");
    let output = command_output_with_timeout(command, Duration::from_secs(120))
        .context("opencode plugin library could not be prepared")?;
    if !output.status.success()
        || !folder
            .join("node_modules/@opencode-ai/plugin/package.json")
            .is_file()
    {
        let bytes = [output.stdout, output.stderr].concat();
        let tail = &bytes[bytes.len().saturating_sub(400)..];
        bail!(
            "opencode plugin library could not be prepared: {}",
            String::from_utf8_lossy(tail)
        );
    }
    Ok(folder)
}

fn copy_sdk(source: &Path, target: &Path) -> Result<()> {
    let source = source.canonicalize()?;
    for name in ["node_modules", "package.json"] {
        let destination = target.join(name);
        if destination.try_exists()? || destination.is_symlink() {
            if destination.is_symlink() {
                bail!("plugin library output has aliases");
            }
            if destination.is_dir() {
                fs::remove_dir_all(&destination)?;
            } else {
                private_file(&destination)?;
                fs::remove_file(&destination)?;
            }
        }
        copy_tree(
            &source.join(name),
            &destination,
            &source,
            &mut BTreeSet::new(),
        )?;
    }
    // Pinned opencode uses bun.lock. Keep compatibility with npm/bun variants
    // without letting a withdrawn lock survive a restore.
    for name in ["bun.lock", "bun.lockb", "package-lock.json"] {
        let destination = target.join(name);
        if destination.try_exists()? || destination.is_symlink() {
            private_file(&destination)?;
            fs::remove_file(&destination)?;
        }
        if source.join(name).try_exists()? {
            copy_tree(
                &source.join(name),
                &destination,
                &source,
                &mut BTreeSet::new(),
            )?;
        }
    }
    Ok(())
}

fn copy_tree(
    path: &Path,
    target: &Path,
    root: &Path,
    ancestors: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let path = path.canonicalize()?;
    if !path.starts_with(root) || !ancestors.insert(path.clone()) {
        bail!("plugin library escapes its root or contains a cycle");
    }
    let metadata = fs::metadata(&path)?;
    if metadata.is_dir() {
        private_directory(target)?;
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            copy_tree(
                &entry.path(),
                &target.join(entry.file_name()),
                root,
                ancestors,
            )?;
        }
    } else if metadata.is_file() {
        let mode = if metadata.mode() & 0o111 != 0 {
            0o700
        } else {
            0o600
        };
        host_write(target, &fs::read(&path)?, mode)?;
    } else {
        bail!("plugin library contains a special file");
    }
    ancestors.remove(&path);
    Ok(())
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn path_quote(path: &Path) -> Result<String> {
    Ok(quote(
        path.to_str()
            .context("opencode launch paths must be UTF-8")?,
    ))
}

fn serve_script(executable: &Path, arguments: &[OsString], state: &Path) -> Result<String> {
    let mut command = vec![path_quote(executable)?];
    for argument in arguments {
        command.push(quote(
            argument.to_str().context("owner argument must be UTF-8")?,
        ));
    }
    Ok(format!("#!/bin/bash\nset -e\nchild=''\ncleanup() {{\n  if [[ -n $child ]]; then /bin/kill -TERM \"$child\" 2>/dev/null || true; wait \"$child\" || true; fi\n}}\ntrap 'cleanup; exit 0' TERM INT HUP\ntrap cleanup EXIT\nstarts=()\nwhile :; do\n  now=$(/bin/date +%s)\n  recent=()\n  for started in \"${{starts[@]}}\"; do\n    if (( now - started < 600 )); then recent+=(\"$started\"); fi\n  done\n  starts=(\"${{recent[@]}}\")\n  if (( ${{#starts[@]}} >= 5 )); then echo 'opencode restart limit reached' >&2; exit 1; fi\n  starts+=(\"$now\")\n  {} &\n  child=$!\n  printf '%s\\n' \"$child\" > {}\n  wait \"$child\" || true\n  child=''\n  /bin/sleep 5\ndone\n", command.join(" "), path_quote(&state.join("serve.pid"))?))
}

fn attach_script(config: &OpencodeConfig, binding: &RuntimeBinding, conversation: &str) -> String {
    format!("#!/bin/bash\nset -eu\npassword=$(/bin/cat {})\nwhile :; do\n  /usr/bin/env -i TERM=\"${{TERM:-xterm-256color}}\" OPENCODE_SERVER_USERNAME=opencode OPENCODE_SERVER_PASSWORD=\"$password\" {} attach {} --session {} || true\n  /bin/sleep 2\ndone\n", quote(&format!("{}/server.secret", binding.state_dir)), quote(&expand_home(&config.binary).display().to_string()), quote(&format!("http://127.0.0.1:{}", binding.port)), quote(conversation))
}

#[cfg(test)]
mod tests;
