//! Durable handoff notes, drafts and owner messages (sm#2337).
use super::*;
use crate::handoff::execute::HandoffNote;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::process::Command;

pub const LIMIT: usize = 262_144;
pub fn fallback_brief(owner: &str) -> String {
    format!("Handoff note: run `sm handoff show` (the note, then {owner}'s messages to this line, verbatim).")
}

pub fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
pub fn note_text(bytes: &[u8], strip_comments: bool) -> Result<String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("handoff note is not valid UTF-8"))?;
    let text = if strip_comments {
        regex::Regex::new(r"(?s)<!--.*?(?:-->|$)")?
            .replace_all(text, "")
            .into_owned()
    } else {
        text.to_owned()
    };
    let text = if strip_comments {
        text.trim().to_owned()
    } else {
        text
    };
    anyhow::ensure!(
        text.len() <= LIMIT,
        "handoff note is {} bytes; the limit is 262144",
        text.len()
    );
    anyhow::ensure!(!text.trim().is_empty(), "handoff note is empty");
    Ok(text)
}

/// Create once, including the repository-local git exclusion shared by worktrees.
pub fn write_draft(path: &Path, text: &str) -> Result<bool> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("draft has no parent"))?;
    fs::create_dir_all(dir)?;
    let root = dir.parent().unwrap_or(dir);
    if let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path", "info/exclude"])
        .output()
    {
        if output.status.success() {
            let git_path = PathBuf::from(String::from_utf8(output.stdout)?.trim());
            let git_path = if git_path.is_absolute() {
                git_path
            } else {
                root.join(git_path)
            };
            let previous = fs::read_to_string(&git_path).unwrap_or_default();
            if !previous.lines().any(|line| line == ".sm/") {
                if let Some(parent) = git_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(git_path)?;
                if !previous.is_empty() && !previous.ends_with('\n') {
                    writeln!(file)?;
                }
                writeln!(file, ".sm/")?;
            }
        }
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(text.as_bytes())?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub fn remove_draft(path: &Path, expected: Option<&str>) -> Result<bool> {
    if !path.exists() {
        return Ok(true);
    }
    if expected.is_some_and(|sha| {
        fs::read_to_string(path)
            .map(|text| digest(&text) != sha)
            .unwrap_or(true)
    }) {
        return Ok(false);
    }
    fs::remove_file(path)?;
    if let Some(dir) = path.parent() {
        let _ = fs::remove_dir(dir);
    }
    Ok(true)
}

/// True when `dir` is inside a repository's main checkout rather than a linked worktree.
fn is_main_checkout(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .is_some_and(|out| {
            let mut lines = out.lines();
            matches!((lines.next(), lines.next()), (Some(a), Some(b)) if a == b)
        })
}

/// The CLI captures these before submission; cleanup cannot remove a changed file.
pub fn remove_source(path: &Path, before: &fs::Metadata) -> (bool, Option<String>) {
    let parent = path.parent().unwrap_or(Path::new("/"));
    if Command::new("git")
        .arg("-C")
        .arg(parent)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(path)
        .output()
        .is_ok_and(|out| out.status.success())
    {
        return (false, Some("git tracks it".into()));
    }
    match fs::metadata(path) {
        Ok(after)
            if before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.len() == after.len()
                && before.modified().ok() == after.modified().ok() => {}
        Ok(_) => {
            return (
                false,
                Some("device, inode, size or mtime changed since the read".into()),
            )
        }
        Err(error) => return (false, Some(error.to_string())),
    }
    match fs::remove_file(path) {
        Ok(()) => (true, None),
        Err(error) => (false, Some(error.to_string())),
    }
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}

impl SessionStore {
    fn handoff_owner_name(&self) -> String {
        self.handoff_config
            .as_ref()
            .map(|c| c.owner_name.clone())
            .unwrap_or_else(|| crate::config::AppConfig::default().owner_name)
    }
    pub fn with_handoff_config(mut self, config: &crate::config::AppConfig) -> Self {
        self.handoff_config = Some(Arc::new(config.clone()));
        self
    }
    fn notes_db(&self) -> Result<Connection> {
        let queue = self
            .queue_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("handoff note store unavailable"))?;
        queue.ensure_schema()?;
        let conn = Connection::open(queue.db_path())?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS handoff_notes (
            session_id TEXT PRIMARY KEY, line_id TEXT NOT NULL, sequence INTEGER NOT NULL,
            text TEXT NOT NULL, source TEXT NOT NULL, source_path TEXT, source_removed INTEGER,
            source_kept_reason TEXT, created_at TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS handoff_notes_line ON handoff_notes(line_id, sequence);
            CREATE TABLE IF NOT EXISTS handoff_drafts(session_id TEXT PRIMARY KEY, path TEXT NOT NULL, sha256 TEXT NOT NULL, written_at TEXT NOT NULL);",
        )?;
        Ok(conn)
    }
    fn line_ids(&self, state: &Value, id: &str) -> Vec<String> {
        let mut root = id.to_owned();
        let mut seen = BTreeSet::new();
        while seen.insert(root.clone()) {
            let Some(parent) = raw_session_object(state, &root)
                .and_then(|s| json_text(s.get("predecessor_session_id")))
            else {
                break;
            };
            root = parent;
        }
        let mut ids = vec![];
        let mut current = Some(root);
        while let Some(id) = current {
            if ids.contains(&id) {
                break;
            }
            current = raw_session_object(state, &id)
                .and_then(|s| json_text(s.get("successor_session_id")));
            ids.push(id);
        }
        ids
    }
    pub(super) fn handoff_items(
        &self,
        state: &Value,
        id: &str,
    ) -> Result<Vec<(String, Vec<String>)>> {
        let mut items = Vec::new();
        if let Some(queue) = &self.queue_store {
            let claims = crate::work_claims::WorkClaimStore::new(queue.db_path().to_path_buf());
            for view in claims.claims_for_session(id, true)? {
                let claim = view.claim;
                if claim.reserved_at.is_some() {
                    continue;
                }
                let key = format!(
                    "{} #{}",
                    if claim.kind == "pr" { "PR" } else { "ticket" },
                    claim.number
                );
                let mut text = key;
                if !view.title.is_empty() {
                    text.push_str(&format!(" — {}", view.title));
                }
                if claim.kind == "pr" && !view.state.is_empty() {
                    text.push_str(&format!(" ({})", view.state));
                }
                items.push((text, vec![claim.number.to_string()]));
            }
            let conn = self.notes_db()?;
            let mut statement = conn.prepare("SELECT pr_number, requested_at FROM codex_review_request_registrations WHERE requester_session_id=?1 AND is_active=1 ORDER BY requested_at,id")?;
            for row in statement.query_map([id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })? {
                let (pr, at) = row?;
                items.push((
                    format!("review of PR #{pr}, requested {at}"),
                    vec![pr.to_string()],
                ));
            }
        }
        if let Some(config) = &self.handoff_config {
            let dir = expand_home(&config.queue_runner_state_dir().to_string_lossy());
            let path = dir.join("queue_runner.db");
            if path.exists() {
                let conn = Connection::open(path)?;
                let mut stmt = conn.prepare("SELECT id,label FROM queue_jobs WHERE (requester_session_id=?1 OR notify_session_id=?1) AND finished_at IS NULL ORDER BY queued_at,id")?;
                for row in stmt.query_map([id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })? {
                    let (job, label) = row?;
                    let label = if label.is_empty() { job.clone() } else { label };
                    items.push((
                        format!(
                            "queue job {label} ({job}), log {}",
                            dir.join("logs").join(format!("{job}.log")).display()
                        ),
                        vec![label, job],
                    ));
                }
            }
        }
        if let Some(sessions) = state["sessions"].as_array() {
            for child in sessions.iter().filter_map(Value::as_object).filter(|s| {
                json_text(s.get("parent_session_id")).as_deref() == Some(id)
                    && !raw_session_is_stopped(s)
            }) {
                let child_id = json_text(child.get("id")).unwrap_or_default();
                let name = raw_session_display_name(child, &child_id);
                items.push((format!("child {name} ({child_id})"), vec![name, child_id]));
            }
        }
        Ok(items)
    }
    pub(super) fn handoff_draft_raw(&self, state: &mut Value, id: &str) -> Result<Value> {
        let session =
            raw_session_object(state, id).ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        let name = raw_session_display_name(session, id);
        let previous = self.handoff_draft_record(id)?;
        // An asked agent edits the file at the recorded path, so a later claim never moves it.
        let (dir, draft_dir) = match &previous {
            Some((path, _)) => {
                let dir = Path::new(path)
                    .parent()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                (dir.clone(), dir)
            }
            None => self.handoff_work_dir(state, id)?,
        };
        anyhow::ensure!(
            dir.is_absolute(),
            "handoff working directory must be absolute"
        );
        let items = self.handoff_items(state, id)?;
        let list = |running: bool| {
            let lines = items
                .iter()
                .filter(|(s, _)| {
                    (s.starts_with("queue job ")
                        || s.starts_with("review of ")
                        || s.starts_with("child "))
                        == running
                })
                .map(|(s, _)| {
                    if running {
                        format!("- {s}\n  → what to do when it lands:")
                    } else {
                        format!("- {s}")
                    }
                })
                .collect::<Vec<_>>();
            if lines.is_empty() {
                "- none".to_owned()
            } else {
                lines.join("\n")
            }
        };
        let branch = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["branch", "--show-current"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(|s| format!(" (branch {s})"))
            .unwrap_or_default();
        let text = format!(
            include_str!("draft.md"),
            owner = self.handoff_owner_name(),
            name = name,
            id = id,
            date = &now_rfc3339()[..10],
            dir = dir.display(),
            branch = branch,
            claims = list(false),
            running = list(true)
        );
        let text = if previous.is_some() {
            session
                .get("handoff_draft_text")
                .and_then(Value::as_str)
                .unwrap_or(&text)
                .to_owned()
        } else {
            text
        };
        let sha = previous
            .map(|(_, sha)| sha)
            .unwrap_or_else(|| digest(&text));
        let path = draft_dir.join(".sm/handoff.md");
        self.notes_db()?.execute("INSERT INTO handoff_drafts(session_id,path,sha256,written_at) VALUES(?1,?2,?3,?4) ON CONFLICT(session_id) DO UPDATE SET path=excluded.path",params![id,path.display().to_string(),sha,now_rfc3339()])?;
        session_object_mut(ensure_sessions_array_mut(state)?, id)
            .unwrap()
            .insert("handoff_draft_text".into(), json!(text));
        Ok(json!({"path":path, "text":text, "sha256":sha}))
    }
    /// The directory the agent works in and the directory its draft goes in.
    /// The work directory is the agent's newest claimed worktree that exists, else its
    /// spawn directory: an agent spawned in a main checkout that then ran
    /// `sm ticket --setup-worktree` works in the worktree. The draft goes there too,
    /// except that sm never writes into a main checkout; when no claimed worktree is
    /// left (its PR merged and sm deleted the worktree), the draft goes under sm's
    /// state directory instead.
    fn handoff_work_dir(&self, state: &Value, id: &str) -> Result<(PathBuf, PathBuf)> {
        let session =
            raw_session_object(state, id).ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        let spawn_dir = expand_home(&json_text(session.get("working_dir")).unwrap_or_default());
        if Self::handoff_node(state, id) != "primary" {
            return Ok((spawn_dir.clone(), spawn_dir));
        }
        let Some(queue) = &self.queue_store else {
            return Ok((spawn_dir.clone(), spawn_dir));
        };
        let claims = crate::work_claims::WorkClaimStore::new(queue.db_path().to_path_buf())
            .claims_for_session(id, true)?;
        // Plain `sm ticket` in a main checkout records that checkout as the claim's path.
        let work_dir = claims
            .into_iter()
            .rev()
            .filter_map(|view| view.claim.worktree_path)
            .map(|path| expand_home(&path))
            .find(|path| path.is_absolute() && path.is_dir())
            .unwrap_or(spawn_dir);
        if !is_main_checkout(&work_dir) {
            return Ok((work_dir.clone(), work_dir));
        }
        let state_dir = queue
            .db_path()
            .parent()
            .unwrap_or(Path::new("/"))
            .join("handoffs")
            .join(id);
        Ok((work_dir, state_dir))
    }
    pub fn handoff_draft(&self, id: &str) -> Result<Value> {
        let _guard = self.write_guard()?;
        self.thaw_lineage_locked(&_guard, &[id])?;
        let mut state = self.load_raw_json_value()?;
        let draft = self.handoff_draft_raw(&mut state, id)?;
        self.write_raw_json_value(&state)?;
        Ok(draft)
    }
    /// Remote file operations use the same node's SSH destination and options.
    pub(super) fn handoff_draft_file(
        &self,
        state: &Value,
        id: &str,
        draft: &Value,
        action: &str,
        expected: Option<&str>,
    ) -> Result<bool> {
        let session =
            raw_session_object(state, id).ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        let node = json_text(session.get("node")).unwrap_or_else(default_node);
        let path = Path::new(draft["path"].as_str().unwrap_or_default());
        if node == "primary" {
            if action == "write" {
                write_draft(path, draft["text"].as_str().unwrap_or_default())?;
                return Ok(true);
            }
            return remove_draft(path, expected);
        }
        Ok(self
            .handoff_run_ssh(
                &node,
                include_str!("draft_remote.py"),
                &json!({"draft":draft,"action":action,"expected":expected}),
            )?
            .as_bool()
            .unwrap_or(false))
    }
    fn handoff_node(state: &Value, id: &str) -> String {
        raw_session_object(state, id)
            .and_then(|s| json_text(s.get("node")))
            .unwrap_or_else(default_node)
    }
    fn handoff_artifact_path(state: &Value, id: &str, path: &str) -> PathBuf {
        if Self::handoff_node(state, id) == "primary" {
            expand_home(path)
        } else {
            PathBuf::from(path)
        }
    }
    fn handoff_run_ssh(&self, node_name: &str, script: &str, payload: &Value) -> Result<Value> {
        let config = self
            .handoff_config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("remote handoff configuration missing"))?;
        let node = config
            .nodes
            .registry
            .get(node_name)
            .ok_or_else(|| anyhow::anyhow!("unknown node {node_name}"))?;
        #[cfg(test)]
        let program = self
            .handoff_ssh_program
            .as_deref()
            .unwrap_or(Path::new("ssh"));
        #[cfg(not(test))]
        let program = Path::new("ssh");
        let mut cmd = Command::new(program);
        cmd.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
        ]);
        if let Some(proxy) = &node.ssh_proxy_command {
            cmd.args(["-o", &format!("ProxyCommand={proxy}")]);
        }
        if let Some(control) = &node.control_path {
            cmd.args(["-o", &format!("ControlPath={control}")]);
        }
        cmd.arg(
            node.ssh
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("node has no ssh destination"))?,
        );
        let quoted = format!("'{}'", script.replace('\'', "'\\''"));
        cmd.arg(format!("python3 -c {quoted}"));
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())?;
        let output = child.wait_with_output()?;
        anyhow::ensure!(
            output.status.success(),
            "handoff operation on node {node_name} failed"
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }
    fn read_handoff_artifact(
        &self,
        state: &Value,
        id: &str,
        path: &Path,
    ) -> Result<Option<(String, Value)>> {
        let node = Self::handoff_node(state, id);
        if node == "primary" {
            let mut file = match fs::File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let metadata = file.metadata()?;
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            let mut source = source_snapshot(id, path, &metadata);
            source["node"] = json!(node);
            return Ok(Some((text, source)));
        }
        let response = self.handoff_run_ssh(
            &node,
            include_str!("artifact_remote.py"),
            &json!({"action":"read","path":path}),
        )?;
        if response["exists"] == false {
            return Ok(None);
        }
        let text = response["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("node returned no artifact text"))?
            .to_owned();
        let mut source = response["snapshot"].clone();
        source["session_id"] = json!(id);
        source["path"] = json!(path);
        source["node"] = json!(node);
        Ok(Some((text, source)))
    }
    fn remove_handoff_original(
        &self,
        state: &Value,
        source: &Value,
    ) -> Result<(bool, Option<String>)> {
        let id = source["session_id"].as_str().unwrap_or_default();
        let node = source["node"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| Self::handoff_node(state, id));
        let path = Path::new(source["path"].as_str().unwrap_or_default());
        if node != "primary" {
            let response = self.handoff_run_ssh(
                &node,
                include_str!("artifact_remote.py"),
                &json!({"action":"remove","path":path,"snapshot":source}),
            )?;
            return Ok((
                response["removed"].as_bool().unwrap_or(false),
                response["reason"].as_str().map(str::to_owned),
            ));
        }
        if !path.exists() {
            return Ok((true, None));
        }
        let before = fs::metadata(path)?;
        if source["device"].as_u64() == Some(before.dev())
            && source["inode"].as_u64() == Some(before.ino())
            && source["size"].as_u64() == Some(before.len())
            && source["mtime"] == metadata_mtime(&before)
        {
            Ok(remove_source(path, &before))
        } else {
            Ok((
                false,
                Some("device, inode, size or mtime changed since the read".into()),
            ))
        }
    }
    pub(super) fn prepare_handoff_ask(
        &self,
        state: &mut Value,
        id: &str,
        text: &str,
        write: bool,
    ) -> String {
        match self.handoff_draft_raw(state, id) {
            Ok(draft) => {
                if !write
                    || self
                        .handoff_draft_file(state, id, &draft, "write", None)
                        .is_ok()
                {
                    text.replace("Post what the next agent needs in your PR, ticket, or a file, then run `sm handoff --link <url>` or `sm handoff --path <file>` and end your turn.", &format!("Your handoff note is drafted at {} with what sm knows. Fill in the rest, re-read it against the checklist at its end, then run `sm handoff` and end your turn.", draft["path"].as_str().unwrap()))
                } else {
                    fallback_ask(text)
                }
            }
            Err(error) => {
                eprintln!("preparing handoff draft for {id}: {error:#}");
                fallback_ask(text)
            }
        }
    }
    pub fn handoff_draft_record(&self, id: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .notes_db()?
            .query_row(
                "SELECT path,sha256 FROM handoff_drafts WHERE session_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
    pub fn remove_handoff_draft_record(&self, id: &str) -> Result<()> {
        self.notes_db()?
            .execute("DELETE FROM handoff_drafts WHERE session_id=?1", [id])?;
        Ok(())
    }
    pub(super) fn cleanup_handoff_draft(&self, state: &Value, id: &str, unchanged_only: bool) {
        let Some(session) = raw_session_object(state, id) else {
            return;
        };
        let record = match self.handoff_draft_record(id) {
            Ok(record) => record,
            Err(error) => {
                eprintln!("reading draft record for {id}: {error:#}");
                return;
            }
        };
        // A shared working directory does not make this session the file's owner.
        let Some((path, sha)) = record else {
            return;
        };
        let other_owner = self.notes_db().and_then(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM handoff_drafts WHERE path=?1 AND session_id<>?2)",
                params![path, id],
                |row| row.get::<_, bool>(0),
            )?)
        });
        if other_owner.unwrap_or(true)
            || (!unchanged_only
                && session
                    .get("successor_session_id")
                    .and_then(Value::as_str)
                    .is_some())
        {
            if !unchanged_only {
                let _ = self.remove_handoff_draft_record(id);
            }
            return;
        }
        let sha = if unchanged_only {
            Some(sha.as_str())
        } else {
            None
        };
        match self.handoff_draft_file(state, id, &json!({"path":path}), "remove", sha) {
            Ok(true) => {
                if let Err(error) = self.remove_handoff_draft_record(id) {
                    eprintln!("removing draft record for {id}: {error:#}");
                }
            }
            Ok(false) => {}
            Err(error) => eprintln!("cleaning handoff draft for {id}: {error:#}"),
        }
    }
    /// The accepted note is durable, and no successor can yet create a draft.
    pub fn cleanup_handoff_before_spawn(&self, id: &str) -> Result<()> {
        let _guard = self.write_guard()?;
        self.thaw_lineage_locked(&_guard, &[id])?;
        let parsed = self.load_parsed_state()?;
        let state = &parsed.raw;
        self.cleanup_handoff_draft(state, id, false);
        Ok(())
    }
    pub(super) fn save_handoff_note(
        &self,
        state: &Value,
        id: &str,
        note: &HandoffNote,
    ) -> Result<()> {
        let ids = self.line_ids(state, id);
        let conn = self.notes_db()?;
        conn.execute("INSERT INTO handoff_notes(session_id,line_id,sequence,text,source,source_path,created_at)
            VALUES (?1,?2,COALESCE((SELECT MAX(sequence)+1 FROM handoff_notes WHERE line_id=?2),1),?3,?4,?5,?6)
            ON CONFLICT(session_id) DO UPDATE SET text=excluded.text,source=excluded.source,source_path=excluded.source_path,source_removed=NULL,source_kept_reason=NULL",
            params![id,ids.first().unwrap_or(&id.to_owned()),note.value,note.source.as_deref().unwrap_or("draft"),note.source_path,now_rfc3339()])?;
        Ok(())
    }
    pub fn report_handoff_source(
        &self,
        id: &str,
        removed: bool,
        reason: Option<&str>,
    ) -> Result<()> {
        self.notes_db()?.execute(
            "UPDATE handoff_notes SET source_removed=?2,source_kept_reason=?3 WHERE session_id=?1",
            params![id, removed, reason],
        )?;
        Ok(())
    }
    pub fn pending_handoff_source_paths(&self) -> Result<Vec<std::path::PathBuf>> {
        let parsed = self.load_parsed_state()?;
        let state = &parsed.raw;
        Ok(state["handoff_note_sources_pending"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["path"].as_str())
            .map(expand_home)
            .collect())
    }
    pub fn handoff_doc_rows(&self) -> Result<BTreeMap<String, Vec<crate::work_history::RowDoc>>> {
        let parsed = self.load_parsed_state()?;
        let state = &parsed.raw;
        let mut by_id = BTreeMap::new();
        for record in state["sessions"].as_array().into_iter().flatten() {
            if let (Some(id), Some(record)) = (record["id"].as_str(), record.as_object()) {
                by_id.entry(id).or_insert(record);
            }
        }
        let mut rows = BTreeMap::<String, Vec<crate::work_history::RowDoc>>::new();
        for s in state["sessions"].as_array().into_iter().flatten() {
            let Some(id) = s["id"].as_str() else { continue };
            let note = &s["handoff"]["note"];
            if !note.is_object() {
                continue;
            }
            let name = raw_session_display_name(s.as_object().unwrap(), id);
            let next = s["successor_session_id"].as_str();
            let next_name = next.map(|n| {
                by_id
                    .get(n)
                    .map(|s| raw_session_display_name(s, n))
                    .unwrap_or_else(|| n.to_owned())
            });
            let superseded = note["superseded_by"].as_str();
            let target = superseded.unwrap_or(id);
            let title = if let Some(latest) = superseded {
                let latest = by_id
                    .get(latest)
                    .map(|s| raw_session_display_name(s, latest))
                    .unwrap_or_else(|| latest.to_owned());
                format!("Handoff note (overwritten by {latest}'s)")
            } else if note["kind"] == "path"
                && !Self::handoff_artifact_path(
                    state,
                    id,
                    note["value"].as_str().unwrap_or_default(),
                )
                .exists()
            {
                "Handoff note (file no longer exists)".to_owned()
            } else {
                format!(
                    "Handoff note{}",
                    next_name.map(|n| format!(" → {n}")).unwrap_or_default()
                )
            };
            let reader_path = if note["kind"] == "link" {
                note["value"].as_str().unwrap_or_default().to_owned()
            } else {
                format!("/docs/~handoff/{target}")
            };
            let row = crate::work_history::RowDoc {
                id: format!("handoff-{id}"),
                name: format!("Handoff note from {name}"),
                title,
                state: "handoff".to_owned(),
                reader_path,
                owner_reviews: 0,
                author_session_id: id.to_owned(),
                published_at: s["handoff"]["accepted_at"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            };
            rows.entry(id.to_owned()).or_default().push(row.clone());
            if let Some(next) = next {
                let mut incoming = row;
                incoming.title = format!("Handoff note from {name}");
                rows.entry(next.to_owned()).or_default().push(incoming);
            }
        }
        Ok(rows)
    }
    pub fn handoff_note_view(&self, target: &str, line: bool, own: bool) -> Result<Value> {
        self.handoff_note_view_inner(target, line, own, true)
    }
    pub fn handoff_note_page(&self, target: &str) -> Result<Value> {
        self.handoff_note_view_inner(target, true, false, false)
    }
    fn handoff_note_view_inner(
        &self,
        target: &str,
        line: bool,
        own: bool,
        messages: bool,
    ) -> Result<Value> {
        let parsed = self.load_parsed_state()?;
        let state = &parsed.raw;
        let sessions = state["sessions"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        let session = sessions
            .iter()
            .find(|s| s["id"] == target || s["friendly_name"] == target || s["name"] == target)
            .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
        let target = if own {
            session["predecessor_session_id"]
                .as_str()
                .unwrap_or(session["id"].as_str().unwrap_or(target))
        } else {
            session["id"].as_str().unwrap_or(target)
        };
        let ids = self.line_ids(state, target);
        let conn = self.notes_db()?;
        let mut notes = vec![];
        let mut count = 0;
        for id in &ids {
            let Some(s) = raw_session_object(state, id) else {
                continue;
            };
            let note = s.get("handoff").and_then(|h| h.get("note"));
            let Some(note) = note.filter(|n| n.is_object()) else {
                continue;
            };
            count += 1;
            if !line && id != target {
                continue;
            }
            let mut entry = json!({"session":{"id":id,"name":raw_session_display_name(s,id)},"successor":null,"sequence":count,"kind":note["kind"],"source":null,"source_path":null,"created_at":s.get("handoff").and_then(|h|h.get("accepted_at"))});
            if let Some(next) = json_text(s.get("successor_session_id")) {
                entry["successor"] = json!({"id":next,"name":raw_session_object(state,&next).map(|s|raw_session_display_name(s,&next)).unwrap_or(next)});
            }
            match note["kind"].as_str() {
                Some("stored") => {
                    let stored_id = note["value"]
                        .as_str()
                        .and_then(|s| s.strip_prefix("handoff_notes:"))
                        .unwrap_or(id);
                    let row = conn.query_row("SELECT text,source,source_path,created_at,sequence FROM handoff_notes WHERE session_id=?1",[stored_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?)))?;
                    entry["text"] = json!(row.0);
                    entry["source"] = json!(row.1);
                    entry["source_path"] = json!(row.2);
                    entry["created_at"] = json!(row.3);
                    entry["sequence"] = json!(row.4);
                }
                Some("link") => entry["url"] = note["value"].clone(),
                Some("path") => {
                    if let Some(latest) = note["superseded_by"].as_str() {
                        entry["superseded_by"] = json!(latest);
                        entry["text"]=json!(format!("Handoff note (overwritten by {latest}'s). Run `sm handoff show {latest}` to read the surviving note."));
                        notes.push(entry);
                        continue;
                    }
                    let path = Self::handoff_artifact_path(
                        state,
                        id,
                        note["value"].as_str().unwrap_or_default(),
                    );
                    entry["source_path"] = json!(path);
                    if let Ok(Some((text, _))) = self.read_handoff_artifact(state, id, &path) {
                        entry["text"] = json!(text);
                    }
                }
                _ => {}
            }
            notes.push(entry);
        }
        let messages = if messages {
            self.handoff_owner_messages(state, &ids)?
        } else {
            vec![]
        };
        Ok(
            json!({"notes":notes,"owner_messages":messages,"line_note_count":count,"owner_name":self.handoff_owner_name()}),
        )
    }
    fn handoff_owner_messages(&self, state: &Value, ids: &[String]) -> Result<Vec<Value>> {
        let conn = self.notes_db()?;
        let mut messages = vec![];
        for id in ids {
            let Some(session) = raw_session_object(state, id) else {
                continue;
            };
            let identity = json!({"id":id,"name":raw_session_display_name(session,id)});
            let mut excluded = BTreeSet::new();
            if table_exists(&conn, "message_queue")? {
                let mut stmt=conn.prepare("SELECT text FROM message_queue WHERE target_session_id=?1 AND delivered_at IS NOT NULL")?;
                for row in stmt.query_map([id], |r| r.get::<_, String>(0))? {
                    excluded.insert(normalize(&row?));
                }
            }
            if table_exists(&conn, "owner_message_replies")? {
                let mut stmt=conn.prepare("SELECT body,delivered_text,created_at FROM owner_message_replies WHERE delivered_to_session_id=?1 ORDER BY created_at,id")?;
                for row in stmt.query_map([id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })? {
                    let (body, delivered, at) = row?;
                    excluded.insert(normalize(&body));
                    excluded.insert(normalize(&delivered));
                    messages.push(
                        json!({"at":at,"source":"inbox reply","session":identity,"text":body}),
                    );
                }
            }
            if let Some(intents) = state["spawn_launch_intents"].as_array() {
                for intent in intents.iter().filter(|i| i["session_id"] == id.as_str()) {
                    if let Some(path) = intent["artifact"]["path"].as_str() {
                        if let Ok(text) = fs::read_to_string(expand_home(path)) {
                            excluded.insert(normalize(&text));
                        }
                    }
                }
            }
            let provider = json_text(session.get("provider")).unwrap_or_default();
            if !matches!(provider.as_str(), "claude" | "codex" | "codex-fork") {
                continue;
            }
            if let Some(path) = json_text(session.get("transcript_path")) {
                let path = Self::handoff_artifact_path(state, id, &path);
                let inputs = if Self::handoff_node(state, id) == "primary" {
                    transcript_messages(&path, &excluded)
                } else {
                    self.handoff_run_ssh(
                        &Self::handoff_node(state, id),
                        include_str!("transcript_remote.py"),
                        &json!({"path":path,"excluded":excluded,"harness_tags":HARNESS_TAGS}),
                    )
                    .and_then(|value| {
                        let inputs = serde_json::from_value::<Vec<(String, String)>>(
                            value["messages"].clone(),
                        )?;
                        // Keep the first non-fitting message's position in the
                        // global merge without transferring its (possibly huge)
                        // body. It always stops selection if reached.
                        if let Some(at) = value["cutoff"].as_str() {
                            messages.push(json!({"at":at,"transcript_cutoff":true}));
                        }
                        Ok(inputs)
                    })
                };
                match inputs {
                    Ok(inputs) => {
                        for (at, text) in inputs {
                            messages.push(
                                json!({"at":at,"source":"terminal","session":identity,"text":text}),
                            );
                        }
                    }
                    Err(error) => eprintln!("handoff transcript for {id}: {error:#}"),
                }
            }
        }
        messages.sort_by(|a, b| a["at"].as_str().cmp(&b["at"].as_str()));
        let mut bytes = 0;
        let mut kept = vec![];
        for message in messages.into_iter().rev() {
            if message["transcript_cutoff"] == true {
                break;
            }
            let size = message["text"].as_str().unwrap_or_default().len();
            if bytes + size > 80_000 && !kept.is_empty() {
                break;
            }
            bytes += size;
            kept.push(message);
        }
        kept.reverse();
        Ok(kept)
    }
}
pub fn fallback_ask(text: &str) -> String {
    if let Some((prefix, _)) = text.split_once("Your handoff note is drafted at ") {
        return format!(
            "{prefix}Run `sm handoff`; it drafts your note with what sm knows and tells you where."
        );
    }
    text.replace("Post what the next agent needs in your PR, ticket, or a file, then run `sm handoff --link <url>` or `sm handoff --path <file>` and end your turn.","Run `sm handoff`; it drafts your note with what sm knows and tells you where.")
}
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn transcript_messages(path: &Path, excluded: &BTreeSet<String>) -> Result<Vec<(String, String)>> {
    transcript_messages_reader(io::BufReader::new(fs::File::open(path)?), excluded)
}
fn transcript_messages_reader(
    reader: impl BufRead,
    excluded: &BTreeSet<String>,
) -> Result<Vec<(String, String)>> {
    let mut result: Vec<(String, String)> = vec![];
    let mut enqueued = BTreeMap::<String, usize>::new();
    let mut last_codex: Option<(String, String)> = None;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(&line)?;
        let kind = event["type"].as_str().unwrap_or_default();
        let queue = kind == "queue-operation" && event["operation"] == "enqueue";
        let user = kind == "user"
            || (kind == "response_item"
                && event["payload"]["type"] == "message"
                && event["payload"]["role"] == "user")
            || (kind == "event_msg" && event["payload"]["type"] == "user_message");
        if !queue && !user {
            continue;
        }
        let content = if queue {
            &event["content"]
        } else if kind == "user" {
            &event["message"]["content"]
        } else if kind == "event_msg" {
            &event["payload"]["message"]
        } else {
            &event["payload"]["content"]
        };
        let text = if let Some(s) = content.as_str() {
            s.to_owned()
        } else if let Some(parts) = content.as_array() {
            parts
                .iter()
                .filter(|p| matches!(p["type"].as_str(), Some("text" | "input_text")))
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            continue;
        };
        let trimmed = text.trim_start();
        let normalized = normalize(&text);
        if normalized.is_empty()
            || ["[Input from: ", "[sm "]
                .iter()
                .any(|prefix| trimmed.starts_with(prefix))
            || HARNESS_TAGS
                .iter()
                .any(|prefix| trimmed.starts_with(prefix))
            || excluded.contains(&normalized)
        {
            continue;
        }
        if queue {
            *enqueued.entry(normalized.clone()).or_default() += 1;
        } else if let Some(count) = enqueued.get_mut(&normalized) {
            if *count > 0 {
                *count -= 1;
                continue;
            }
        }
        if matches!(kind, "response_item" | "event_msg") {
            if last_codex
                .as_ref()
                .is_some_and(|(previous_kind, previous_text)| {
                    previous_kind != kind && previous_text == &normalized
                })
            {
                last_codex = None;
                continue;
            }
            last_codex = Some((kind.into(), normalized));
        }
        result.push((
            event["timestamp"].as_str().unwrap_or_default().to_owned(),
            text,
        ));
    }
    Ok(result)
}

pub fn render_notes(view: &Value) -> String {
    let mut lines = vec![];
    for note in view["notes"].as_array().into_iter().flatten() {
        let name = note["session"]["name"].as_str().unwrap_or_default();
        if let Some(url) = note["url"].as_str() {
            lines.push(format!("{name} handed off with a link: {url}"));
        } else {
            lines.push(format!(
                "=== Handoff note from {name} ({}) ===\n{}",
                note["session"]["id"].as_str().unwrap_or_default(),
                note["text"]
                    .as_str()
                    .unwrap_or("Handoff note (file no longer exists)")
            ));
        }
    }
    if view["owner_messages"]
        .as_array()
        .is_some_and(|m| !m.is_empty())
    {
        lines.push(format!(
            "=== {}'s messages to this line, verbatim (oldest first) ===",
            view["owner_name"].as_str().unwrap_or("Owner")
        ));
        for message in view["owner_messages"].as_array().unwrap() {
            lines.push(format!(
                "[{} · {} · {}] {}",
                message["at"].as_str().unwrap_or_default(),
                message["source"].as_str().unwrap_or_default(),
                message["session"]["name"].as_str().unwrap_or_default(),
                message["text"].as_str().unwrap_or_default()
            ));
        }
    }
    if view["line_note_count"].as_u64().unwrap_or(0) > 1 {
        lines.push("Earlier notes in this line: sm handoff show --line".into());
    }
    lines.join("\n")
}

/// Extend only with a transcript fixture demonstrating the injected tag.
pub const HARNESS_TAGS: &[&str] = &[
    "<environment_context>",
    "<user_instructions>",
    "<task-notification>",
    "<system-reminder>",
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
    "<user-prompt-submit-hook>",
];

pub(super) fn missing_items(items: &[(String, Vec<String>)], text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    items
        .iter()
        .filter(|(_, keys)| {
            !keys.iter().any(|key| {
                let key = key.to_lowercase();
                if key.bytes().all(|b| b.is_ascii_digit()) {
                    lower.match_indices(&key).any(|(at, _)| {
                        let word = |c: char| c.is_alphanumeric() || c == '_';
                        !lower[..at].chars().next_back().is_some_and(word)
                            && !lower[at + key.len()..].chars().next().is_some_and(word)
                    })
                } else {
                    lower.contains(&key)
                }
            })
        })
        .map(|(item, _)| item.split(", log ").next().unwrap_or(item).to_owned())
        .collect()
}

impl SessionStore {
    /// Import steps are restartable across the SQLite and JSON stores.
    pub fn maintain_handoff_notes(&self) -> Result<()> {
        self.maintain_handoff_notes_checkpoint(|_| Ok(()))
    }
    fn maintain_handoff_notes_checkpoint(
        &self,
        mut checkpoint: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let mut state = self.load_raw_json_value()?;
        let before = state.clone();
        let importing = state.get("handoff_notes_imported_at").is_none();
        let mut pending = state["handoff_note_sources_pending"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if importing {
            let mut paths = BTreeMap::<(String, PathBuf), Vec<String>>::new();
            if let Some(sessions) = state["sessions"].as_array() {
                for session in sessions {
                    let path = if session["handoff"]["note"]["kind"] == "path" {
                        session["handoff"]["note"]["value"].as_str()
                    } else {
                        session["last_handoff_path"].as_str().filter(|p| {
                            expand_home(p).starts_with(
                                self.state_file
                                    .parent()
                                    .unwrap_or(Path::new("/"))
                                    .join("handoffs"),
                            )
                        })
                    };
                    if let (Some(path), Some(id)) = (path, session["id"].as_str()) {
                        paths
                            .entry((
                                Self::handoff_node(&state, id),
                                Self::handoff_artifact_path(&state, id, path),
                            ))
                            .or_default()
                            .push(id.into());
                    }
                }
            }
            // A prior attempt may already have rewritten the latest record or removed
            // its original. Include its row so an older shared path never steals custody.
            let conn = self.notes_db()?;
            let mut stmt = conn.prepare(
                "SELECT session_id,source_path FROM handoff_notes WHERE source='import'",
            )?;
            for row in
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (id, path) = row?;
                paths
                    .entry((
                        Self::handoff_node(&state, &id),
                        Self::handoff_artifact_path(&state, &id, &path),
                    ))
                    .or_default()
                    .push(id);
            }
            for ((_node, path), mut ids) in paths {
                ids.sort_by_key(|id| {
                    self.line_ids(&state, id)
                        .iter()
                        .position(|s| s == id)
                        .unwrap_or(0)
                });
                ids.dedup();
                let id = ids.last().unwrap().clone();
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM handoff_notes WHERE session_id=?1)",
                    [&id],
                    |r| r.get(0),
                )?;
                if !exists {
                    let Some((raw_text, source)) =
                        self.read_handoff_artifact(&state, &id, &path)?
                    else {
                        continue;
                    };
                    let text = note_text(raw_text.as_bytes(), false)?;
                    pending.retain(|s| s["session_id"] != id);
                    pending.push(source);
                    state["handoff_note_sources_pending"] = json!(pending);
                    self.write_raw_json_value(&state)?;
                    checkpoint("source")?;
                    let line = self.line_ids(&state, &id);
                    conn.execute("INSERT OR IGNORE INTO handoff_notes(session_id,line_id,sequence,text,source,source_path,created_at) VALUES(?1,?2,COALESCE((SELECT MAX(sequence)+1 FROM handoff_notes WHERE line_id=?2),1),?3,'import',?4,?5)",params![id,line.first().unwrap_or(&id),text,path.display().to_string(),now_rfc3339()])?;
                }
                checkpoint("row")?;
                for older in ids.iter().filter(|s| **s != id) {
                    if let Some(session) =
                        session_object_mut(ensure_sessions_array_mut(&mut state)?, older)
                    {
                        session["handoff"]["note"]["superseded_by"] = json!(id);
                    }
                }
                if let Some(session) =
                    session_object_mut(ensure_sessions_array_mut(&mut state)?, &id)
                {
                    if !session.get("handoff").is_some_and(Value::is_object) {
                        session.insert(
                            "handoff".into(),
                            json!({"state":"done","trigger":"voluntary"}),
                        );
                    }
                    session["handoff"]["note"] =
                        json!({"kind":"stored","value":format!("handoff_notes:{id}")});
                }
                if !pending.iter().any(|s| s["session_id"] == id) {
                    if let Some((_, source)) = self.read_handoff_artifact(&state, &id, &path)? {
                        pending.push(source);
                    } else {
                        pending.push(json!({"session_id":id,"path":path}));
                    }
                }
                state["handoff_note_sources_pending"] = json!(pending);
                self.write_raw_json_value(&state)?;
                checkpoint("rewrite")?;
            }
        }
        let mut remaining = vec![];
        let merged = self.load_parsed_state()?;
        for source in pending {
            let Some(id) = source["session_id"].as_str() else {
                continue;
            };
            let ids = self.line_ids(&merged.raw, id);
            let ended = ids
                .last()
                .and_then(|id| raw_session_object(&merged.raw, id))
                .is_some_and(raw_session_is_stopped);
            if !ended {
                remaining.push(source);
                continue;
            }
            let (removed, reason) = self.remove_handoff_original(&merged.raw, &source)?;
            self.report_handoff_source(id, removed, reason.as_deref())?;
            checkpoint("remove")?;
        }
        state["handoff_note_sources_pending"] = json!(remaining);
        if importing {
            state["handoff_notes_imported_at"] = json!(now_rfc3339());
        }
        if state != before {
            self.write_raw_json_value(&state)?;
        }
        checkpoint("marker")?;
        let legacy = self
            .state_file
            .parent()
            .unwrap_or(Path::new("/"))
            .join("handoffs");
        if legacy.exists()
            && !remaining.iter().any(|s| {
                s["path"]
                    .as_str()
                    .is_some_and(|p| Path::new(p).starts_with(&legacy))
            })
        {
            match fs::remove_dir(legacy) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
    pub fn handoff_line_id(&self, target: &str) -> Result<Option<String>> {
        let parsed = self.load_parsed_state()?;
        let state = &parsed.raw;
        let id = state["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["id"] == target || s["friendly_name"] == target || s["name"] == target)
            .and_then(|s| s["id"].as_str());
        Ok(id.and_then(|id| self.line_ids(state, id).first().cloned()))
    }
}
fn source_snapshot(id: &str, path: &Path, before: &fs::Metadata) -> Value {
    json!({"session_id":id,"path":path,"device":before.dev(),"inode":before.ino(),"size":before.len(),"mtime":metadata_mtime(before)})
}
fn metadata_mtime(metadata: &fs::Metadata) -> Value {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| json!(d.as_nanos().to_string()))
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::execute::NoteKind;
    use crate::sessions::HandoffAcceptOutcome;
    fn fixture() -> (SessionStore, PathBuf) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "handoff-custody-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let store =
            SessionStore::new_with_queue(root.join("sessions.json"), root.join("messages.db"));
        store.write_raw_json_value(&json!({"sessions":[{
            "id":"agent001","name":"test-agent","friendly_name":"test-agent","provider":"claude",
            "working_dir":root,"status":"working","created_at":"2026-10-19T00:00:00Z","last_activity":"2026-10-19T00:00:00Z"
        }]})).unwrap();
        store.notes_db().unwrap();
        crate::work_claims::WorkClaimStore::new(root.join("messages.db"))
            .ensure_schema()
            .unwrap();
        (store, root)
    }
    fn text_note(text: &str) -> HandoffNote {
        HandoffNote {
            kind: NoteKind::Text,
            value: text.into(),
            source: Some("draft".into()),
            source_path: None,
        }
    }
    #[test]
    fn handoff_doc_rows_cover_links_missing_files_and_overwritten_notes() {
        let (store, root) = fixture();
        let mut state = store.load_raw_json_value().unwrap();
        let mut next = state["sessions"][0].clone();
        next["id"] = json!("agent002");
        next["name"] = json!("next");
        next["friendly_name"] = json!("next");
        next["predecessor_session_id"] = json!("agent001");
        state["sessions"][0]["successor_session_id"] = json!("agent002");
        state["sessions"][0]["handoff"] =
            json!({"note":{"kind":"link","value":"https://github.com/owner/repo/issues/1"}});
        state["sessions"].as_array_mut().unwrap().push(next);
        store.write_raw_json_value(&state).unwrap();
        let rows = store.handoff_doc_rows().unwrap();
        assert_eq!(rows["agent001"][0].title, "Handoff note → next");
        assert_eq!(rows["agent002"][0].title, "Handoff note from test-agent");
        assert_eq!(
            rows["agent001"][0].reader_path,
            "https://github.com/owner/repo/issues/1"
        );
        state["sessions"][0]["handoff"]["note"] =
            json!({"kind":"path","value":root.join("gone.md")});
        store.write_raw_json_value(&state).unwrap();
        assert_eq!(
            store.handoff_doc_rows().unwrap()["agent001"][0].title,
            "Handoff note (file no longer exists)"
        );
        state["sessions"][0]["handoff"]["note"]["superseded_by"] = json!("agent002");
        store.write_raw_json_value(&state).unwrap();
        let row = &store.handoff_doc_rows().unwrap()["agent001"][0];
        assert_eq!(row.title, "Handoff note (overwritten by next's)");
        assert_eq!(row.reader_path, "/docs/~handoff/agent002");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_draft_excluded_once_and_never_overwrites_edits() {
        let (store, root) = fixture();
        assert!(Command::new("git")
            .arg("init")
            .arg(&root)
            .output()
            .unwrap()
            .status
            .success());
        let draft = store.handoff_draft("agent001").unwrap();
        // The draft itself goes outside a main checkout; write_draft still excludes `.sm/`
        // in whatever checkout it writes to.
        let path = &root.join(".sm/handoff.md");
        assert!(draft["text"]
            .as_str()
            .unwrap()
            .contains(&format!("## {}'s instructions", store.handoff_owner_name())));
        assert!(store.load_raw_json_value().unwrap()["sessions"][0]
            .get("handoff")
            .is_none());
        assert!(write_draft(path, draft["text"].as_str().unwrap()).unwrap());
        assert_eq!(digest(&fs::read_to_string(path).unwrap()), draft["sha256"]);
        fs::write(path, "agent edits").unwrap();
        assert!(!write_draft(path, "replacement").unwrap());
        assert_eq!(fs::read_to_string(path).unwrap(), "agent edits");
        let exclude = fs::read_to_string(root.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|s| *s == ".sm/").count(), 1);
        remove_draft(path, draft["sha256"].as_str()).unwrap();
        assert!(path.exists());
        remove_draft(path, None).unwrap();
        assert!(!root.join(".sm").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_draft_goes_to_claimed_worktree_and_stays_there() {
        let (store, root) = fixture();
        let claim = |id: &str, number: i64, worktree: &Path, at: &str| {
            fs::create_dir_all(worktree).unwrap();
            Connection::open(root.join("messages.db"))
                .unwrap()
                .execute(
                    "INSERT INTO work_claims(id,repo,number,kind,session_id,source,worktree_path,claimed_at) VALUES(?1,'o/r',?2,'ticket','agent001','explicit',?3,?4)",
                    params![id, number, worktree.display().to_string(), at],
                )
                .unwrap();
        };
        claim("c1", 7, &root.join("wt-7"), "2026-10-19T01:00:00Z");
        let draft = store.handoff_draft("agent001").unwrap();
        assert_eq!(draft["path"], json!(root.join("wt-7/.sm/handoff.md")));
        assert!(draft["text"].as_str().unwrap().contains(&format!(
            "Working directory: {}",
            root.join("wt-7").display()
        )));
        // A later claim never moves a draft the agent may already be editing.
        claim("c2", 8, &root.join("wt-8"), "2026-10-19T02:00:00Z");
        let again = store.handoff_draft("agent001").unwrap();
        assert_eq!(again["path"], draft["path"]);
        // Without a recorded draft, the newest existing claimed worktree wins.
        store.remove_handoff_draft_record("agent001").unwrap();
        let fresh = store.handoff_draft("agent001").unwrap();
        assert_eq!(fresh["path"], json!(root.join("wt-8/.sm/handoff.md")));
        fs::remove_dir_all(root.join("wt-8")).unwrap();
        store.remove_handoff_draft_record("agent001").unwrap();
        let fallback = store.handoff_draft("agent001").unwrap();
        assert_eq!(fallback["path"], json!(root.join("wt-7/.sm/handoff.md")));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_draft_never_lands_in_a_main_checkout() {
        let (store, root) = fixture();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q"]);
        // The agent's PR merged, so its claim ended and sm deleted the worktree.
        let draft = store.handoff_draft("agent001").unwrap();
        let path = root.join("handoffs/agent001/.sm/handoff.md");
        assert_eq!(draft["path"], json!(&path));
        assert!(draft["text"]
            .as_str()
            .unwrap()
            .contains(&format!("Working directory: {}", root.display())));
        assert!(!root.join(".sm").exists());
        // A plain `sm ticket` claim records the main checkout itself; still not there.
        store.remove_handoff_draft_record("agent001").unwrap();
        Connection::open(root.join("messages.db"))
            .unwrap()
            .execute(
                "INSERT INTO work_claims(id,repo,number,kind,session_id,source,worktree_path,claimed_at) VALUES('c1','o/r',7,'ticket','agent001','explicit',?1,'2026-10-19T01:00:00Z')",
                params![root.display().to_string()],
            )
            .unwrap();
        assert_eq!(
            store.handoff_draft("agent001").unwrap()["path"],
            json!(path)
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_refuses_missing_new_child_once_then_stores_and_replaces_sequence() {
        let (store, root) = fixture();
        store.handoff_draft("agent001").unwrap();
        let mut state = store.load_raw_json_value().unwrap();
        state["sessions"].as_array_mut().unwrap().push(json!({"id":"child001","name":"worker","parent_session_id":"agent001","status":"working"}));
        store.write_raw_json_value(&state).unwrap();
        assert!(
            matches!(store.accept_handoff("agent001","agent001",&text_note("Next: finish")).unwrap(),HandoffAcceptOutcome::Incomplete(m) if m==vec!["child worker (child001)"])
        );
        let warned = store.load_raw_json_value().unwrap();
        assert!(warned["sessions"][0].get("handoff").is_none());
        assert!(matches!(
            store
                .accept_handoff("agent001", "agent001", &text_note("Next: finish"))
                .unwrap(),
            HandoffAcceptOutcome::Accepted { .. }
        ));
        assert_eq!(
            store.load_raw_json_value().unwrap()["sessions"][0]["handoff"]["note"]["kind"],
            "stored"
        );
        store
            .accept_handoff("agent001", "agent001", &text_note("worker: continue"))
            .unwrap();
        let view = store.handoff_note_view("test-agent", false, false).unwrap();
        assert_eq!(view["notes"][0]["text"], "worker: continue");
        assert_eq!(view["notes"][0]["sequence"], 1);
        assert_eq!(view["line_note_count"], 1);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_item_matching_has_number_boundaries_and_case_insensitive_names() {
        let items = vec![
            ("ticket #22".into(), vec!["22".into()]),
            ("PR #33".into(), vec!["33".into()]),
            (
                "queue job smoke".into(),
                vec!["smoke".into(), "job_1".into()],
            ),
            ("review of PR #44".into(), vec!["44".into()]),
            ("child worker".into(), vec!["worker".into()]),
        ];
        assert!(missing_items(&items, "#22 33 SMOKE #44 WORKER").is_empty());
        assert_eq!(
            missing_items(&items, "122 330 smoke 440 worker"),
            vec!["ticket #22", "PR #33", "review of PR #44"]
        );
    }
    #[test]
    fn handoff_limits_comments_and_source_cleanup() {
        assert_eq!(
            note_text(b"<!--hint-->\n body <!--end-->", true).unwrap(),
            "body"
        );
        assert_eq!(
            note_text(b"<!--hint-->body", false).unwrap(),
            "<!--hint-->body"
        );
        assert!(note_text(&[255], false)
            .unwrap_err()
            .to_string()
            .contains("UTF-8"));
        assert!(note_text(&vec![b'x'; LIMIT + 1], false).is_err());
        assert!(note_text(&vec![b'x'; LIMIT], false).is_ok());
        let (_, root) = fixture();
        let file = root.join("note.md");
        fs::write(&file, "old").unwrap();
        let before = fs::metadata(&file).unwrap();
        fs::write(&file, "changed").unwrap();
        assert!(!remove_source(&file, &before).0);
        assert!(Command::new("git")
            .arg("init")
            .arg(&root)
            .output()
            .unwrap()
            .status
            .success());
        assert!(Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "note.md"])
            .output()
            .unwrap()
            .status
            .success());
        assert_eq!(
            remove_source(&file, &fs::metadata(&file).unwrap()),
            (false, Some("git tracks it".into()))
        );
        let other = root.join("other.md");
        fs::write(&other, "ok").unwrap();
        assert!(remove_source(&other, &fs::metadata(&other).unwrap()).0);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_terminal_filter_preserves_html_and_deduplicates_enqueue() {
        let (_, root) = fixture();
        let transcript = root.join("claude.jsonl");
        fs::write(&transcript, include_str!("fixtures/owner-claude.jsonl")).unwrap();
        let excluded = BTreeSet::from(["sm supplied brief".into(), "already delivered".into()]);
        let inputs = transcript_messages(&transcript, &excluded).unwrap();
        assert_eq!(
            inputs.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
            vec!["<div>Keep my HTML</div>", "Human queued message"]
        );
        fs::write(&transcript, "{bad json}").unwrap();
        assert!(transcript_messages(&transcript, &excluded).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_owner_messages_merge_inbox_and_transcripts_keep_newest_cap() {
        let (store, root) = fixture();
        let transcript = root.join("rollout.jsonl");
        fs::write(&transcript, include_str!("fixtures/owner-codex.jsonl")).unwrap();
        let mut state = store.load_raw_json_value().unwrap();
        state["sessions"][0]["provider"] = json!("codex-fork");
        state["sessions"][0]["transcript_path"] = json!(transcript);
        let conn = store.notes_db().unwrap();
        conn.execute_batch("CREATE TABLE owner_message_replies(body TEXT,delivered_text TEXT,delivered_to_session_id TEXT,created_at TEXT,id TEXT);").unwrap();
        conn.execute("INSERT INTO owner_message_replies VALUES(?1,?2,'agent001','2026-10-19T02:00:00Z','r1')",params!["Inbox words","inbox envelope"]).unwrap();
        let messages = store
            .handoff_owner_messages(&state, &["agent001".into()])
            .unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|m| m["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["Terminal words", "Inbox words"]
        );
        // A malformed transcript contributes nothing; the inbox remains available.
        fs::write(&transcript, "invalid").unwrap();
        assert_eq!(
            store
                .handoff_owner_messages(&state, &["agent001".into()])
                .unwrap()
                .len(),
            1
        );
        conn.execute("INSERT INTO owner_message_replies VALUES(?1,'x','agent001','2026-10-19T01:00:00Z','old')",["a".repeat(80_000)]).unwrap();
        assert_eq!(
            store
                .handoff_owner_messages(&state, &["agent001".into()])
                .unwrap()
                .len(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_import_shared_source_keeps_older_provenance_and_defers_live_cleanup() {
        let (store, root) = fixture();
        let source = root.join("shared.md");
        fs::write(&source, "surviving note").unwrap();
        let mut state = store.load_raw_json_value().unwrap();
        state["sessions"][0]["status"] = json!("stopped");
        state["sessions"][0]["successor_session_id"] = json!("agent002");
        state["sessions"][0]["handoff"] =
            json!({"state":"done","trigger":"voluntary","note":{"kind":"path","value":source}});
        let mut next = state["sessions"][0].clone();
        next["id"] = json!("agent002");
        next["predecessor_session_id"] = json!("agent001");
        next["successor_session_id"] = Value::Null;
        next["status"] = json!("working");
        state["sessions"].as_array_mut().unwrap().push(next);
        store.write_raw_json_value(&state).unwrap();
        store.maintain_handoff_notes().unwrap();
        let state = store.load_raw_json_value().unwrap();
        assert_eq!(state["sessions"][0]["handoff"]["note"]["kind"], "path");
        assert_eq!(
            state["sessions"][0]["handoff"]["note"]["superseded_by"],
            "agent002"
        );
        assert_eq!(state["sessions"][1]["handoff"]["note"]["kind"], "stored");
        assert!(source.exists());
        store.maintain_handoff_notes().unwrap();
        assert_eq!(
            store
                .notes_db()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM handoff_notes", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let mut state = state;
        state["sessions"][1]["status"] = json!("stopped");
        store.write_raw_json_value(&state).unwrap();
        store.maintain_handoff_notes().unwrap();
        assert!(!source.exists());
        assert_eq!(
            store.handoff_note_view("agent002", false, false).unwrap()["notes"][0]["text"],
            "surviving note"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_import_recovers_after_each_durable_step() {
        for stage in ["source", "row", "rewrite", "remove", "marker"] {
            let (store, root) = fixture();
            let path = root.join("import.md");
            fs::write(&path, "keep forever").unwrap();
            let mut state = store.load_raw_json_value().unwrap();
            state["sessions"][0]["status"] = json!("stopped");
            state["sessions"][0]["handoff"] =
                json!({"state":"done","trigger":"voluntary","note":{"kind":"path","value":path}});
            store.write_raw_json_value(&state).unwrap();
            assert!(store
                .maintain_handoff_notes_checkpoint(|step| {
                    anyhow::ensure!(step != stage, "simulated crash at {step}");
                    Ok(())
                })
                .is_err());
            let restarted =
                SessionStore::new_with_queue(root.join("sessions.json"), root.join("messages.db"));
            restarted.maintain_handoff_notes().unwrap();
            assert!(!path.exists(), "{stage}");
            assert_eq!(
                restarted
                    .handoff_note_view("agent001", false, false)
                    .unwrap()["notes"][0]["text"],
                "keep forever"
            );
            assert!(restarted
                .load_raw_json_value()
                .unwrap()
                .get("handoff_notes_imported_at")
                .is_some());
            assert_eq!(
                restarted
                    .notes_db()
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM handoff_notes", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn handoff_server_writes_before_asking_and_preserves_an_edited_draft() {
        let (store, root) = fixture();
        let mut state = store.load_raw_json_value().unwrap();
        let old = crate::handoff::policy::ask_text(
            &crate::handoff::policy::AskReason::Context { percent: 36.0 },
            "none",
        );
        let ask = store.prepare_handoff_ask(&mut state, "agent001", &old, true);
        let path = root.join(".sm/handoff.md");
        assert!(path.exists());
        assert!(ask.contains(&format!(
            "Your handoff note is drafted at {}",
            path.display()
        )));
        fs::write(&path, "keep edits").unwrap();
        store.cleanup_handoff_draft(&state, "agent001", true);
        assert!(path.exists());
        store.cleanup_handoff_draft(&state, "agent001", false);
        assert!(!path.exists());
        // Missing remote SSH configuration falls back to a CLI-created draft.
        state["sessions"][0]["node"] = json!("remote");
        let ask = store.prepare_handoff_ask(&mut state, "agent001", &old, true);
        assert!(ask.contains("Run `sm handoff`; it drafts your note"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_draft_row_survives_dirty_withdrawal_and_is_removed_with_file() {
        let (store, root) = fixture();
        let mut state = store.load_raw_json_value().unwrap();
        let draft = store.handoff_draft_raw(&mut state, "agent001").unwrap();
        let path = Path::new(draft["path"].as_str().unwrap());
        write_draft(path, draft["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            store.handoff_draft_record("agent001").unwrap().unwrap().1,
            draft["sha256"]
        );
        fs::write(path, "edited").unwrap();
        store.cleanup_handoff_draft(&state, "agent001", true);
        assert!(store.handoff_draft_record("agent001").unwrap().is_some());
        fs::write(path, draft["text"].as_str().unwrap()).unwrap();
        store.cleanup_handoff_draft(&state, "agent001", true);
        assert!(store.handoff_draft_record("agent001").unwrap().is_none());
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_resubmit_keeps_saved_note_source_sequence_and_cleanup_outcome() {
        let (store, root) = fixture();
        let mut note = text_note("  Saved next step.\n");
        note.source = Some("file".into());
        note.source_path = Some("/original.md".into());
        store.accept_handoff("agent001", "agent001", &note).unwrap();
        store
            .report_handoff_source("agent001", false, Some("git tracks it"))
            .unwrap();
        let mut state = store.load_raw_json_value().unwrap();
        state["sessions"][0]["handoff"]["state"] = json!("failed");
        store.write_raw_json_value(&state).unwrap();
        assert!(matches!(
            store.resubmit_handoff("agent001", "other").unwrap(),
            HandoffAcceptOutcome::Forbidden
        ));
        assert!(matches!(
            store.resubmit_handoff("agent001", "agent001").unwrap(),
            HandoffAcceptOutcome::Accepted { .. }
        ));
        let conn = store.notes_db().unwrap();
        let row = conn
            .query_row(
                "SELECT text,source,sequence,source_removed FROM handoff_notes",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row, ("  Saved next step.\n".into(), "file".into(), 1, 0));
        assert!(store.handoff_draft_record("agent001").unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_file_limit_counts_raw_whitespace_and_newest_message_is_whole() {
        assert!(note_text(&vec![b' '; LIMIT + 1], false)
            .unwrap_err()
            .to_string()
            .contains("262145"));
        assert_eq!(note_text(b"  raw text\n", false).unwrap(), "  raw text\n");
        let (store, root) = fixture();
        let conn = store.notes_db().unwrap();
        conn.execute_batch("CREATE TABLE owner_message_replies(body TEXT,delivered_text TEXT,delivered_to_session_id TEXT,created_at TEXT,id TEXT);").unwrap();
        conn.execute("INSERT INTO owner_message_replies VALUES(?1,'x','agent001','2026-10-19T01:00:00Z','new')",["a".repeat(80_004)]).unwrap();
        let messages = store
            .handoff_owner_messages(&store.load_raw_json_value().unwrap(), &["agent001".into()])
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["text"].as_str().unwrap().len(), 80_004);
        fs::remove_dir_all(root).unwrap();
    }
    fn replace_same_size_mtime(path: &Path) {
        let held = fs::File::open(path).unwrap();
        let before = held.metadata().unwrap();
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, vec![b'x'; before.len() as usize]).unwrap();
        fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        fs::rename(replacement, path).unwrap();
        let after = fs::metadata(path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_ne!(before.ino(), after.ino());
    }
    #[test]
    fn handoff_source_replacement_with_same_size_and_mtime_is_kept() {
        let (_, root) = fixture();
        let path = root.join("note.md");
        fs::write(&path, "original").unwrap();
        let held = fs::File::open(&path).unwrap();
        let before = held.metadata().unwrap();
        replace_same_size_mtime(&path);
        assert!(!remove_source(&path, &before).0);
        assert!(path.exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_import_keeps_replacement_after_crash_and_finishes_missing_sources() {
        let (store, root) = fixture();
        let path = root.join("note.md");
        fs::write(&path, "original").unwrap();
        let mut state = store.load_raw_json_value().unwrap();
        state["sessions"][0]["status"] = json!("stopped");
        state["sessions"][0]["handoff"] =
            json!({"state":"done","trigger":"voluntary","note":{"kind":"path","value":path}});
        store.write_raw_json_value(&state).unwrap();
        assert!(store
            .maintain_handoff_notes_checkpoint(|stage| {
                if stage == "row" {
                    replace_same_size_mtime(&path);
                    anyhow::bail!("simulated crash");
                }
                Ok(())
            })
            .is_err());
        store.maintain_handoff_notes().unwrap();
        assert!(path.exists());
        assert_eq!(
            store.handoff_note_view("agent001", false, false).unwrap()["notes"][0]["text"],
            "original"
        );
        fs::remove_dir_all(root).unwrap();
        let (store, root) = fixture();
        let mut state = store.load_raw_json_value().unwrap();
        let note = json!({"kind":"path","value":root.join("gone.md")});
        state["sessions"][0]["handoff"] = json!({"state":"done","trigger":"voluntary","note":note});
        store.write_raw_json_value(&state).unwrap();
        store.maintain_handoff_notes().unwrap();
        let state = store.load_raw_json_value().unwrap();
        assert_eq!(state["sessions"][0]["handoff"]["note"], note);
        assert!(state.get("handoff_notes_imported_at").is_some());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_predecessor_retirement_preserves_the_successors_new_draft() {
        for stale_predecessor_row in [false, true] {
            let (store, root) = fixture();
            let old = store.handoff_draft("agent001").unwrap();
            let path = Path::new(old["path"].as_str().unwrap());
            write_draft(path, old["text"].as_str().unwrap()).unwrap();
            remove_draft(path, None).unwrap();
            if !stale_predecessor_row {
                store.remove_handoff_draft_record("agent001").unwrap();
            }
            let mut state = store.load_raw_json_value().unwrap();
            let mut successor = state["sessions"][0].clone();
            successor["id"] = json!("agent002");
            successor["name"] = json!("successor");
            successor["friendly_name"] = json!("successor");
            successor["predecessor_session_id"] = json!("agent001");
            state["sessions"][0]["successor_session_id"] = json!("agent002");
            state["sessions"].as_array_mut().unwrap().push(successor);
            store.write_raw_json_value(&state).unwrap();
            let new = store.handoff_draft("agent002").unwrap();
            write_draft(path, new["text"].as_str().unwrap()).unwrap();
            fs::write(path, "successor's edited note").unwrap();
            store.cleanup_handoff_draft(&state, "agent001", false);
            assert_eq!(fs::read_to_string(path).unwrap(), "successor's edited note");
            assert!(store.handoff_draft_record("agent002").unwrap().is_some());
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn handoff_legacy_directory_cleanup_keeps_changed_and_tracked_sources() {
        for tracked in [false, true] {
            let (store, root) = fixture();
            let legacy = root.join("handoffs");
            fs::create_dir(&legacy).unwrap();
            let source = legacy.join("note.md");
            fs::write(&source, "original").unwrap();
            if tracked {
                assert!(Command::new("git")
                    .arg("init")
                    .arg(&root)
                    .output()
                    .unwrap()
                    .status
                    .success());
                assert!(Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["add", "handoffs/note.md"])
                    .output()
                    .unwrap()
                    .status
                    .success());
            }
            let mut state = store.load_raw_json_value().unwrap();
            state["sessions"][0]["status"] = json!("stopped");
            state["sessions"][0]["handoff"] =
                json!({"state":"done","trigger":"voluntary","note":{"kind":"path","value":source}});
            store.write_raw_json_value(&state).unwrap();
            store
                .maintain_handoff_notes_checkpoint(|stage| {
                    if stage == "row" && !tracked {
                        fs::write(&source, "keep changed source")?;
                    }
                    Ok(())
                })
                .unwrap();
            assert!(source.exists());
            assert!(legacy.exists());
            store.maintain_handoff_notes().unwrap();
            assert!(source.exists());
            assert_eq!(
                store.handoff_note_view("agent001", false, false).unwrap()["notes"][0]["text"],
                "original"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn remote_transcript_filter_matches_local_and_bounds_transfer() {
        let (_store, root) = fixture();
        let path = root.join("remote.jsonl");
        let excluded = BTreeSet::from(["Inbox words".into(), "inbox envelope".into()]);
        let extract = || {
            let mut child = Command::new("python3")
                .args(["-c", include_str!("transcript_remote.py")])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(
                    json!({"path":path,"excluded":excluded,"harness_tags":HARNESS_TAGS})
                        .to_string()
                        .as_bytes(),
                )
                .unwrap();
            child.wait_with_output().unwrap()
        };
        let mut file = fs::File::create(&path).unwrap();
        for fixture in [
            include_str!("fixtures/owner-claude.jsonl"),
            include_str!("fixtures/owner-codex.jsonl"),
        ] {
            writeln!(file, "{fixture}").unwrap();
        }
        file.flush().unwrap();
        let output = extract();
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_value::<Vec<(String, String)>>(
                serde_json::from_slice::<Value>(&output.stdout).unwrap()["messages"].clone()
            )
            .unwrap(),
            {
                let mut messages = transcript_messages(&path, &excluded).unwrap();
                messages.sort_by(|a, b| a.0.cmp(&b.0));
                messages
            }
        );
        // Tool traffic dominates the input, but contributes no returned bytes.
        for i in 0..40 {
            writeln!(
                file,
                "{}",
                json!({"type":"assistant","message":{"content":"t".repeat(250_000)}})
            )
            .unwrap();
            writeln!(file, "{}", json!({"type":"user","timestamp":format!("2026-10-19T02:{i:02}:00Z"),"message":{"content":format!("  Owner {i}: é\n{}", "x".repeat(4_000))}})).unwrap();
        }
        // Nonmonotonic timestamps must preserve the cap's first-nonfit barrier.
        writeln!(
            file,
            "{}",
            json!({"type":"user","timestamp":"2026-10-19T00:00:00Z","message":{"content":"older"}})
        )
        .unwrap();
        drop(file);
        let expected = || {
            let mut all = transcript_messages(&path, &excluded).unwrap();
            all.sort_by(|a, b| a.0.cmp(&b.0));
            let mut bytes = 0;
            let mut kept = vec![];
            for message in all.into_iter().rev() {
                if bytes + message.1.len() > 80_000 && !kept.is_empty() {
                    break;
                }
                bytes += message.1.len();
                kept.push(message);
            }
            kept.reverse();
            kept
        };
        let output = extract();
        assert!(output.status.success());
        assert!(output.stdout.len() < 90_000);
        assert!(fs::metadata(&path).unwrap().len() > 10_000_000);
        assert_eq!(
            serde_json::from_value::<Vec<(String, String)>>(
                serde_json::from_slice::<Value>(&output.stdout).unwrap()["messages"].clone()
            )
            .unwrap(),
            expected()
        );
        let oversized = "é".repeat(40_002);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"user","timestamp":"2026-10-20","message":{"content":oversized}})
        )
        .unwrap();
        drop(file);
        let output = extract();
        assert!(output.status.success());
        assert!(output.stdout.len() < oversized.len() + 100);
        assert_eq!(
            serde_json::from_value::<Vec<(String, String)>>(
                serde_json::from_slice::<Value>(&output.stdout).unwrap()["messages"].clone()
            )
            .unwrap(),
            vec![("2026-10-20".into(), oversized)]
        );
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{bad json}}").unwrap();
        drop(file);
        let output = extract();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        for invalid in [
            r#"{"type":"assistant","value":NaN}"#,
            r#"{"type":"assistant","value":"\ud800"}"#,
            r#"{"type":"assistant","value":1e999}"#,
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(transcript_messages(&path, &excluded).is_err());
            let output = extract();
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
        }
        fs::remove_file(&path).unwrap();
        let output = extract();
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["messages"],
            json!([])
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn handoff_remote_notes_transcripts_and_import_stay_on_their_session_node() {
        use std::os::unix::fs::PermissionsExt;
        let (mut store, root) = fixture();
        let remote_root = root.join("remote-fs");
        fs::create_dir_all(&remote_root).unwrap();
        let ssh = root.join("ssh-stub");
        fs::write(
            &ssh,
            format!(
                r#"#!/usr/bin/env python3
import json, os, subprocess, sys
root = {remote_root:?}
r = json.load(sys.stdin)
if 'path' in r:
    r['path'] = os.path.join(root, r['path'].lstrip('/'))
if 'draft' in r:
    r['draft']['path'] = os.path.join(root, r['draft']['path'].lstrip('/'))
p = subprocess.run(sys.argv[-1], shell=True, input=json.dumps(r).encode())
sys.exit(p.returncode)
"#,
                remote_root = remote_root.display().to_string()
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = crate::config::AppConfig::default();
        config.nodes.registry.insert(
            "remote".into(),
            crate::config::NodeConfig {
                id: "remote".into(),
                ssh: Some("remote.example".into()),
                ..Default::default()
            },
        );
        store = store.with_handoff_config(&config);
        store.handoff_ssh_program = Some(ssh);
        let note_path = root.join("shared-note.md");
        fs::write(&note_path, "primary note").unwrap();
        let remote_note = remote_root.join(note_path.strip_prefix("/").unwrap());
        fs::create_dir_all(remote_note.parent().unwrap()).unwrap();
        fs::write(&remote_note, "remote note").unwrap();
        let transcript = "/provider/rollout.jsonl";
        let remote_transcript = remote_root.join(transcript.trim_start_matches('/'));
        fs::create_dir_all(remote_transcript.parent().unwrap()).unwrap();
        fs::write(&remote_transcript,"{\"type\":\"user\",\"timestamp\":\"2026-10-19T00:00:00Z\",\"message\":{\"content\":\"Owner's remote instruction\"}}\n").unwrap();
        let mut state = store.load_raw_json_value().unwrap();
        state["sessions"][0]["node"] = json!("remote");
        state["sessions"][0]["status"] = json!("stopped");
        state["sessions"][0]["transcript_path"] = json!(transcript);
        state["sessions"][0]["handoff"] =
            json!({"state":"done","trigger":"voluntary","note":{"kind":"path","value":note_path}});
        let mut local = state["sessions"][0].clone();
        local["id"] = json!("local001");
        local["name"] = json!("local");
        local["friendly_name"] = json!("local");
        local["node"] = json!("primary");
        local["provider"] = json!("codex-app");
        local["status"] = json!("working");
        state["sessions"].as_array_mut().unwrap().push(local);
        store.write_raw_json_value(&state).unwrap();
        let view = store.handoff_note_view("agent001", false, false).unwrap();
        assert_eq!(view["notes"][0]["text"], "remote note");
        assert_eq!(
            view["owner_messages"][0]["text"],
            "Owner's remote instruction"
        );
        // A remote non-fitting message remains a stopping point after merging
        // with earlier Inbox replies, even though its body is not transferred.
        let conn = store.notes_db().unwrap();
        conn.execute_batch("CREATE TABLE owner_message_replies(body TEXT,delivered_text TEXT,delivered_to_session_id TEXT,created_at TEXT,id TEXT);").unwrap();
        conn.execute("INSERT INTO owner_message_replies VALUES('older inbox','envelope','agent001','2026-10-18','r1')", []).unwrap();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&remote_transcript)
            .unwrap();
        for (at, text) in [
            ("2026-10-19T01", "x".repeat(79_000)),
            ("2026-10-19T02", "y".repeat(2_000)),
        ] {
            writeln!(
                file,
                "{}",
                json!({"type":"user","timestamp":at,"message":{"content":text}})
            )
            .unwrap();
        }
        drop(file);
        let view = store.handoff_note_view("agent001", false, false).unwrap();
        assert_eq!(view["owner_messages"].as_array().unwrap().len(), 1);
        assert_eq!(view["owner_messages"][0]["text"], "y".repeat(2_000));
        let draft = json!({"path":"/draft-work/.sm/handoff.md","text":"remote draft"});
        store
            .handoff_draft_file(&state, "agent001", &draft, "write", None)
            .unwrap();
        assert!(remote_root.join("draft-work/.sm/handoff.md").exists());
        store
            .handoff_draft_file(&state, "agent001", &draft, "remove", None)
            .unwrap();
        assert!(!remote_root.join("draft-work/.sm/handoff.md").exists());
        store.maintain_handoff_notes().unwrap();
        assert!(!remote_note.exists());
        assert_eq!(fs::read_to_string(&note_path).unwrap(), "primary note");
        assert_eq!(
            store.handoff_note_view("agent001", false, false).unwrap()["notes"][0]["text"],
            "remote note"
        );
        assert_eq!(
            store.handoff_note_view("local001", false, false).unwrap()["notes"][0]["text"],
            "primary note"
        );
        assert_eq!(
            store
                .notes_db()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM handoff_notes", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        fs::remove_dir_all(root).unwrap();
    }
}
