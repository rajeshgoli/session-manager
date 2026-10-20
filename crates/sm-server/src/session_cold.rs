//! Crash-safe movement between the hot registry and its ordered cold segment.
use super::*;
use std::ops::Deref;

const FIELDS: [&str; 2] = ["sessions", "session_runtime_launches"];

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(super) struct ColdEntry {
    anchor: Option<String>,
    record: Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct ColdSegment {
    version: u32,
    sessions: Vec<ColdEntry>,
    session_runtime_launches: Vec<ColdEntry>,
    #[serde(skip)]
    indexes: OnceLock<ColdIndexes>,
}

#[derive(Debug, Default)]
struct ColdIndexes {
    by_id: BTreeMap<String, usize>,
    by_reference: BTreeMap<(String, String), BTreeSet<String>>,
}

impl Default for ColdSegment {
    fn default() -> Self {
        Self {
            version: 1,
            sessions: Vec::new(),
            session_runtime_launches: Vec::new(),
            indexes: OnceLock::new(),
        }
    }
}

impl ColdSegment {
    fn indexes(&self) -> &ColdIndexes {
        self.indexes.get_or_init(|| {
            let mut indexes = ColdIndexes::default();
            for (idx, entry) in self.sessions.iter().enumerate() {
                let id = entry.record["id"].as_str().expect("validated cold id");
                indexes.by_id.insert(id.to_owned(), idx);
                if let Some(record) = entry.record.as_object() {
                    for (field, value) in record {
                        if let Some(value) = value.as_str() {
                            indexes
                                .by_reference
                                .entry((field.clone(), value.to_owned()))
                                .or_default()
                                .insert(id.to_owned());
                        }
                    }
                }
            }
            indexes
        })
    }

    fn session(&self, id: &str) -> Option<&Value> {
        self.indexes()
            .by_id
            .get(id)
            .map(|idx| &self.sessions[*idx].record)
    }
    fn entries(&self, field: &str) -> &[ColdEntry] {
        if field == "sessions" {
            &self.sessions
        } else {
            &self.session_runtime_launches
        }
    }

    fn merge(&self, hot: &Value) -> Value {
        let mut merged = hot.clone();
        for field in FIELDS {
            let cold = self.entries(field);
            if cold.is_empty() {
                continue;
            }
            let records = hot[field].as_array().map(Vec::as_slice).unwrap_or_default();
            let by_id: BTreeMap<_, _> = records
                .iter()
                .filter_map(|r| Some((r["id"].as_str()?, r)))
                .collect();
            let cold_ids: BTreeSet<_> = cold
                .iter()
                .filter_map(|e| e.record["id"].as_str())
                .collect();
            let mut groups: BTreeMap<Option<&str>, Vec<&ColdEntry>> = BTreeMap::new();
            for entry in cold {
                let anchor = entry.anchor.as_deref().filter(|id| {
                    if by_id.contains_key(id) {
                        true
                    } else {
                        crate::eprintln!("cold segment: missing anchor {id}");
                        false
                    }
                });
                groups.entry(anchor).or_default().push(entry);
            }
            let mut result = Vec::with_capacity(records.len() + cold.len());
            let mut emitted = BTreeSet::new();
            let mut append = |entries: Vec<&ColdEntry>, result: &mut Vec<Value>| {
                for entry in entries {
                    let id = entry.record["id"].as_str().expect("validated cold id");
                    if emitted.insert(id.to_owned()) {
                        result.push(by_id.get(id).copied().unwrap_or(&entry.record).clone());
                    }
                }
            };
            append(groups.remove(&None).unwrap_or_default(), &mut result);
            for record in records {
                let id = record["id"].as_str();
                if !id.is_some_and(|id| cold_ids.contains(id)) {
                    result.push(record.clone());
                }
                append(groups.remove(&id).unwrap_or_default(), &mut result);
            }
            merged[field] = Value::Array(result);
        }
        merged
    }
}

/// Building the full raw view is deferred until a reader asks for it. Ordinary
/// hot writes neither copy cold JSON nor serialize it under the write lock.
#[derive(Debug)]
pub(super) struct MergedRaw {
    pub(super) hot: Value,
    cold: Arc<ColdSegment>,
    merged: OnceLock<Value>,
}

impl MergedRaw {
    pub(super) fn new(hot: Value, cold: Arc<ColdSegment>) -> Self {
        Self {
            hot,
            cold,
            merged: OnceLock::new(),
        }
    }
}

impl Deref for MergedRaw {
    type Target = Value;
    fn deref(&self) -> &Value {
        if self.cold.sessions.is_empty() && self.cold.session_runtime_launches.is_empty() {
            return &self.hot;
        }
        self.merged.get_or_init(|| {
            let mut raw = self.cold.merge(&self.hot);
            if let Some(sessions) = raw["sessions"].as_array_mut() {
                for record in sessions {
                    normalize_opencode_runtime_record(record);
                }
            }
            raw
        })
    }
}

impl PartialEq for MergedRaw {
    fn eq(&self, other: &Self) -> bool {
        self.deref() == other.deref()
    }
}
impl PartialEq<Value> for MergedRaw {
    fn eq(&self, other: &Value) -> bool {
        self.deref() == other
    }
}

#[derive(Debug)]
pub(super) struct CachedColdState {
    stamp: Option<StateFileStamp>,
    segment: Arc<ColdSegment>,
}

impl SessionStore {
    pub(super) fn cold_reference_ids(
        &self,
        fields: &[&str],
        value: &str,
    ) -> Result<BTreeSet<String>> {
        let (_, cold) = self.load_cold_segment()?;
        Ok(fields
            .iter()
            .flat_map(|field| {
                cold.indexes()
                    .by_reference
                    .get(&(field.to_string(), value.to_owned()))
                    .into_iter()
                    .flatten()
                    .cloned()
            })
            .collect())
    }
    pub(super) fn allocate_runtime_launch_id(
        &self,
        records: &[SessionRuntimeLaunchRecord],
    ) -> Result<String> {
        let (_, cold) = self.load_cold_segment()?;
        loop {
            let id = generate_unique_runtime_launch_id(records)?;
            if !cold
                .session_runtime_launches
                .iter()
                .any(|entry| entry.record["id"] == id)
            {
                return Ok(id);
            }
        }
    }
    /// Called only after serving starts and the previous server has exited.
    pub fn enable_cold_sweeps(&self, serving_since: Instant) {
        *self
            .shared
            .cold_sweep_not_before
            .lock()
            .expect("cold sweep gate poisoned") = Some(serving_since + Duration::from_secs(3600));
    }

    pub fn sweep_cold_sessions_if_ready(&self) -> Result<usize> {
        let ready = *self
            .shared
            .cold_sweep_not_before
            .lock()
            .map_err(|_| anyhow::anyhow!("cold sweep gate poisoned"))?;
        if ready.is_none_or(|deadline| Instant::now() < deadline) {
            return Ok(0);
        }
        // Handover seals this gate and waits for in-flight moves, just as it
        // waits for runtime launches, before passing the listeners.
        let _handover = crate::handover::runtime_launch_gate().begin()?;
        self.sweep_cold_sessions()
    }

    fn cold_path(&self) -> PathBuf {
        self.state_file.with_file_name("sessions.cold.json")
    }

    pub(super) fn load_cold_segment(&self) -> Result<(Option<StateFileStamp>, Arc<ColdSegment>)> {
        validate_session_state_path(&self.state_file)?;
        let path = self.cold_path();
        let mut file = match fs::File::open(&path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let stamp = file
            .as_ref()
            .map(|f| f.metadata().map(|m| StateFileStamp::of(&m)))
            .transpose()?;
        let mut cache = self
            .shared
            .cold_state
            .lock()
            .map_err(|_| anyhow::anyhow!("cold cache lock poisoned"))?;
        if let Some(cached) = cache.as_ref().filter(|c| c.stamp == stamp) {
            return Ok((stamp, cached.segment.clone()));
        }
        let segment = if let Some(file) = file.as_mut() {
            let segment: ColdSegment = serde_json::from_reader(file)
                .with_context(|| format!("parse {}", path.display()))?;
            anyhow::ensure!(
                segment.version == 1,
                "unsupported cold segment version {}",
                segment.version
            );
            for field in FIELDS {
                let mut seen = BTreeSet::new();
                for entry in segment.entries(field) {
                    let id = entry.record["id"]
                        .as_str()
                        .context("cold record has no string id")?;
                    anyhow::ensure!(seen.insert(id), "duplicate cold {field} id {id}");
                }
            }
            segment
        } else {
            ColdSegment::default()
        };
        let segment = Arc::new(segment);
        *cache = Some(CachedColdState {
            stamp,
            segment: segment.clone(),
        });
        Ok((stamp, segment))
    }

    /// The caller's guard spans thaw and its subsequent read/modify/write.
    pub(super) fn thaw_locked(
        &self,
        _guard: &std::sync::MutexGuard<'_, ()>,
        select: impl Fn(&Value) -> bool,
    ) -> Result<usize> {
        self.thaw_locked_checkpoint(select, &mut |_| Ok(()))
    }

    /// Ordinary writes need their targets and direct notification recipients.
    /// Do not thaw a live session's entire historical predecessor chain.
    pub(super) fn thaw_ids_locked(
        &self,
        guard: &std::sync::MutexGuard<'_, ()>,
        targets: &[&str],
    ) -> Result<usize> {
        self.thaw_related_locked(guard, targets, false)
    }

    /// Handoff note mutations also walk predecessor/successor lineage.
    pub(super) fn thaw_lineage_locked(
        &self,
        guard: &std::sync::MutexGuard<'_, ()>,
        targets: &[&str],
    ) -> Result<usize> {
        self.thaw_related_locked(guard, targets, true)
    }

    fn thaw_related_locked(
        &self,
        guard: &std::sync::MutexGuard<'_, ()>,
        targets: &[&str],
        lineage: bool,
    ) -> Result<usize> {
        let (_, cold) = self.load_cold_segment()?;
        if cold.sessions.is_empty() {
            return Ok(0);
        }
        let parsed = self.load_parsed_state()?;
        let records: BTreeMap<_, _> = parsed.raw.hot["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| Some((r["id"].as_str()?, r)))
            .collect();
        let mut ids: BTreeSet<String> = targets.iter().map(|id| (*id).to_owned()).collect();
        let mut pending: Vec<String> = ids.iter().cloned().collect();
        while let Some(id) = pending.pop() {
            if let Some(record) = records
                .get(id.as_str())
                .copied()
                .or_else(|| cold.session(&id))
            {
                let fields: &[&str] = if lineage {
                    &[
                        "parent_session_id",
                        "predecessor_session_id",
                        "successor_session_id",
                        "context_monitor_notify",
                    ]
                } else {
                    &["parent_session_id", "context_monitor_notify"]
                };
                for field in fields {
                    if let Some(next) = record[*field].as_str() {
                        if ids.insert(next.to_owned()) && lineage {
                            pending.push(next.to_owned());
                        }
                    }
                }
            }
        }
        self.thaw_locked(guard, |r| {
            r["id"].as_str().is_some_and(|id| ids.contains(id))
        })
    }

    fn thaw_locked_checkpoint(
        &self,
        select: impl Fn(&Value) -> bool,
        checkpoint: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<usize> {
        let (_, cold) = self.load_cold_segment()?;
        let ids: BTreeSet<_> = cold
            .sessions
            .iter()
            .filter(|e| select(&e.record))
            .filter_map(|e| json_text(e.record.get("id")))
            .collect();
        if ids.is_empty() {
            return Ok(0);
        }
        let parsed = self.load_parsed_state()?;
        let remaining: BTreeSet<_> = cold
            .sessions
            .iter()
            .filter_map(|e| json_text(e.record.get("id")))
            .filter(|id| !ids.contains(id))
            .collect();
        let (hot, cold) = partition(&parsed.raw, &remaining, &cold);
        self.move_cold_files(&hot, &cold, false, checkpoint)?;
        Ok(ids.len())
    }

    /// Run only from the guarded daily maintenance pass, after startup gating.
    pub fn sweep_cold_sessions(&self) -> Result<usize> {
        let _guard = self.write_guard()?;
        self.sweep_cold_checkpoint(OffsetDateTime::now_utc(), &mut |_| Ok(()))
    }

    fn sweep_cold_checkpoint(
        &self,
        now: OffsetDateTime,
        checkpoint: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<usize> {
        let (_, previous) = self.load_cold_segment()?;
        let parsed = self.load_parsed_state()?;
        let state = &*parsed.raw;
        let mut cold_ids: BTreeSet<_> = previous
            .sessions
            .iter()
            .filter_map(|e| json_text(e.record.get("id")))
            .collect();
        let mut blocked = BTreeSet::new();
        if let Some(id) = json_text(state.get("maintainer_session_id")) {
            blocked.insert(id);
        }
        for entry in state["agent_registrations"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(id) = json_text(entry.get("session_id")) {
                blocked.insert(id);
            }
        }
        for request in reparent_request_records(state)?
            .into_iter()
            .filter(ReparentRequestRecord::is_active)
        {
            blocked.extend(request.affected_session_ids());
        }
        for launch in state["session_runtime_launches"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if !matches!(launch["status"].as_str(), Some("applied" | "failed")) {
                if let Some(id) = json_text(launch.get("session_id")) {
                    blocked.insert(id);
                }
            }
        }
        for record in state["sessions"].as_array().into_iter().flatten() {
            if record
                .as_object()
                .is_some_and(|r| !raw_session_is_stopped(r))
            {
                for field in [
                    "parent_session_id",
                    "predecessor_session_id",
                    "successor_session_id",
                ] {
                    if let Some(id) = json_text(record.get(field)) {
                        blocked.insert(id);
                    }
                }
            }
        }
        let mut eligible = Vec::new();
        for record in state["sessions"].as_array().into_iter().flatten() {
            let Some(id) = record["id"].as_str() else {
                continue;
            };
            let Some(object) = record.as_object() else {
                continue;
            };
            if cold_ids.contains(id)
                || blocked.contains(id)
                || !raw_session_is_stopped(object)
                || !matches!(
                    record["completion_status"].as_str(),
                    Some("retired" | "killed")
                )
                || record["is_em"] == true
                || state["handoff_deferred_sessions"]
                    .as_object()
                    .is_some_and(|o| o.contains_key(id))
            {
                continue;
            }
            let stopped = ["stopped_at", "completed_at", "last_activity"]
                .into_iter()
                .find_map(|field| record[field].as_str().and_then(parse_timestamp));
            if let Some(stopped) = stopped.filter(|s| *s < now - TimeDuration::days(14)) {
                eligible.push((stopped, id.to_owned()));
            }
        }
        // Stable sort preserves the registry's order for equal stop times.
        eligible.sort_by_key(|(stopped, _)| *stopped);
        let count = eligible.len().min(1000);
        cold_ids.extend(eligible.into_iter().take(1000).map(|(_, id)| id));
        if cold_ids.is_empty() {
            return Ok(0);
        }
        let (hot, mut cold) = partition(state, &cold_ids, &previous);
        for entry in &mut cold.sessions {
            handoff_transfer::strip_legacy_handoff_record(&mut entry.record);
        }
        self.move_cold_files(&hot, &cold, true, checkpoint)?;
        Ok(count)
    }

    fn move_cold_files(
        &self,
        hot: &Value,
        cold: &ColdSegment,
        sweep: bool,
        checkpoint: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        // Readers cannot combine the old cold file with the new hot file.
        // Invalidate before either write, including on an injected failure.
        let mut cache = self
            .shared
            .parsed_state
            .lock()
            .map_err(|_| anyhow::anyhow!("session cache lock poisoned"))?;
        *cache = None;
        let cold_value = serde_json::to_value(cold)?;
        let hot_path = &self.state_file;
        let cold_path = self.cold_path();
        let writes = if sweep {
            [(&cold_path, &cold_value), (hot_path, hot)]
        } else {
            [(hot_path, hot), (&cold_path, &cold_value)]
        };
        for (path, value) in writes {
            durable_write(path, value, checkpoint)?;
        }
        Ok(())
    }

    /// Offline rollback: the CLI holds the handover socket throughout this call.
    pub fn thaw_all_cold_sessions(&self) -> Result<usize> {
        let _guard = self.write_guard()?;
        let (_, cold) = self.load_cold_segment()?;
        let count = cold.sessions.len();
        if !cold.sessions.is_empty() || !cold.session_runtime_launches.is_empty() {
            let parsed = self.load_parsed_state()?;
            self.move_cold_files(&parsed.raw, &ColdSegment::default(), false, &mut |_| Ok(()))?;
        }
        let path = self.cold_path();
        match fs::remove_file(&path) {
            Ok(()) => {
                fs::File::open(path.parent().context("cold file has no parent")?)?.sync_all()?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(count)
    }
}

/// Rebuild both arrays from the merged order. This handles crashed moves too:
/// hot content wins, and each duplicate remains at its old cold position.
fn partition(
    state: &Value,
    cold_ids: &BTreeSet<String>,
    previous: &ColdSegment,
) -> (Value, ColdSegment) {
    let mut hot = state.clone();
    let mut cold = ColdSegment::default();
    for field in FIELDS {
        let mut hot_records = Vec::new();
        let mut cold_records = Vec::new();
        let mut anchor = None;
        for record in state[field].as_array().into_iter().flatten() {
            let goes_cold = if field == "sessions" {
                record["id"]
                    .as_str()
                    .is_some_and(|id| cold_ids.contains(id))
            } else {
                // Cold launches follow their session; orphan hot launches stay hot.
                record["session_id"]
                    .as_str()
                    .is_some_and(|id| cold_ids.contains(id))
                    || (previous
                        .session_runtime_launches
                        .iter()
                        .any(|e| e.record["id"] == record["id"])
                        && !state["sessions"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|s| s["id"] == record["session_id"]))
            };
            if goes_cold {
                cold_records.push(ColdEntry {
                    anchor: anchor.clone(),
                    record: record.clone(),
                });
            } else {
                anchor = json_text(record.get("id"));
                hot_records.push(record.clone());
            }
        }
        if state.get(field).is_some() || !cold_records.is_empty() {
            hot[field] = Value::Array(hot_records);
        }
        if field == "sessions" {
            cold.sessions = cold_records;
        } else {
            cold.session_runtime_launches = cold_records;
        }
    }
    (hot, cold)
}

fn durable_write(
    path: &Path,
    value: &Value,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let parent = path.parent().context("state file has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!(
        "json.move.{}.{}",
        std::process::id(),
        STATE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    checkpoint("write")?;
    file.sync_all()?;
    checkpoint("file-sync")?;
    fs::rename(&tmp, path)?;
    checkpoint("rename")?;
    fs::File::open(parent)?.sync_all()?;
    checkpoint("directory-sync")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(label: &str) -> (SessionStore, Value) {
        let dir = env::temp_dir().join(format!(
            "cold-{label}-{}-{}",
            std::process::id(),
            STATE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let records = ["A", "X", "Y", "B", "Z"].map(|id| json!({"id":id,"name":"duplicate-name","provider":"claude","working_dir":"/repo","tmux_session":id,"created_at":"2025-01-01T00:00:00Z","last_activity":"2025-01-01T00:00:00Z","stopped_at":"2025-01-01T00:00:00Z","status":"stopped","completion_status":"retired","is_em":id=="A" || id=="B"}));
        let raw = json!({"sessions":records,"session_runtime_launches":[
            {"id":"LA","session_id":"A","status":"applied"},
            {"id":"LX","session_id":"X","status":"applied"},
            {"id":"LY","session_id":"Y","status":"failed"},
            {"id":"LB","session_id":"B","status":"applied"},
            {"id":"LZ","session_id":"Z","status":"applied"}]});
        let store = SessionStore::new(dir.join("sessions.json"));
        store.write_raw_json_value(&raw).unwrap();
        (store, raw)
    }

    #[test]
    fn cold_reserved_hot_filename_is_rejected_before_any_mutation() {
        let (store, raw) = fixture("path-collision");
        let dir = store.state_file.parent().unwrap();
        let reserved = dir.join("sessions.cold.json");
        let original = raw.to_string();
        fs::write(&reserved, &original).unwrap();
        let alias = dir.join("alias.json");
        std::os::unix::fs::symlink(&reserved, &alias).unwrap();
        for path in [&reserved, &alias] {
            let bad = SessionStore::new(path.clone());
            assert!(bad
                .list_sessions(true)
                .unwrap_err()
                .to_string()
                .contains("reserved"));
            assert!(bad
                .write_raw_json_value(&json!({"sessions":[]}))
                .unwrap_err()
                .to_string()
                .contains("reserved"));
            assert!(bad
                .sweep_cold_sessions()
                .unwrap_err()
                .to_string()
                .contains("reserved"));
            assert_eq!(fs::read_to_string(&reserved).unwrap(), original);
            let config = dir.join("invalid.yaml");
            fs::write(
                &config,
                format!("paths:\n  state_file: {}\n", path.display()),
            )
            .unwrap();
            assert!(crate::config::AppConfig::load_from_path(&config)
                .unwrap_err()
                .to_string()
                .contains("reserved"));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// Read-only live input, copied into test isolation before any movement.
    /// Run explicitly through sm queue with SM_COLD_PERF_INPUT set.
    #[test]
    #[ignore = "requires an explicit state-file copy for before/after measurement"]
    fn cold_copy_write_benchmark() {
        let input = PathBuf::from(env::var("SM_COLD_PERF_INPUT").expect("SM_COLD_PERF_INPUT"));
        let (store, _) = fixture("perf");
        fs::copy(&input, &store.state_file).unwrap();
        let cold_input = input.with_file_name("sessions.cold.json");
        if cold_input.exists() {
            fs::copy(cold_input, store.cold_path()).unwrap();
        }
        store.thaw_all_cold_sessions().unwrap();
        store.strip_legacy_handoff_fields().unwrap();
        let before = (*store.load_parsed_state().unwrap().raw).clone();
        let measure = || {
            let started = Instant::now();
            for _ in 0..20 {
                let _guard = store.write_guard().unwrap();
                let hot = store.load_raw_json_value().unwrap();
                store.write_raw_json_value(&hot).unwrap();
            }
            (
                started.elapsed().as_secs_f64() * 1000.0 / 20.0,
                fs::metadata(&store.state_file).unwrap().len(),
            )
        };
        let (before_ms, before_bytes) = measure();
        let mut moved = Vec::new();
        for _ in 0..10 {
            let count = store.sweep_cold_sessions().unwrap();
            moved.push(count);
            if count == 0 {
                break;
            }
        }
        assert_eq!(*store.load_parsed_state().unwrap().raw, before);
        let (after_ms, after_bytes) = measure();
        println!(
            "cold-copy-perf {}",
            json!({"sessions":before["sessions"].as_array().unwrap().len(),"sweeps":moved,"before_bytes":before_bytes,"after_bytes":after_bytes,"before_write_ms":before_ms,"after_write_ms":after_ms})
        );
        fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
    }

    fn ids(value: &Value, field: &str) -> Vec<String> {
        value[field]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_owned())
            .collect()
    }

    fn assert_restart(store: &SessionStore, before: &Value) {
        let restarted = SessionStore::new(store.state_file.clone());
        for field in FIELDS {
            assert_eq!(
                ids(&restarted.load_parsed_state().unwrap().raw, field),
                ids(before, field)
            );
        }
    }

    #[test]
    fn cold_cache_refreshes_after_cold_edit_and_hot_writes_do_not_clone_cold() {
        let (store, _) = fixture("cache");
        store.sweep_cold_sessions().unwrap();
        let mut hot = store.load_raw_json_value().unwrap();
        hot["test"] = json!(true);
        store.write_raw_json_value(&hot).unwrap();
        let parsed = store.load_parsed_state().unwrap();
        assert!(parsed.raw.merged.get().is_none());
        assert_eq!(ids(&parsed.raw, "sessions"), ["A", "X", "Y", "B", "Z"]);
        let cold_path = store.cold_path();
        let mut cold: Value = serde_json::from_slice(&fs::read(&cold_path).unwrap()).unwrap();
        cold["sessions"][0]["record"]["name"] = json!("edited externally");
        fs::write(&cold_path, serde_json::to_vec(&cold).unwrap()).unwrap();
        assert_eq!(
            store.load_parsed_state().unwrap().raw["sessions"][1]["name"],
            "edited externally"
        );
        fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
    }

    #[test]
    fn cold_first_sweep_requires_handover_and_one_hour() {
        let (store, _) = fixture("gate");
        assert_eq!(store.sweep_cold_sessions_if_ready().unwrap(), 0);
        store.enable_cold_sweeps(Instant::now());
        assert_eq!(store.sweep_cold_sessions_if_ready().unwrap(), 0);
        store.enable_cold_sweeps(Instant::now() - Duration::from_secs(3601));
        assert_eq!(store.sweep_cold_sessions_if_ready().unwrap(), 3);
        fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
    }

    #[test]
    fn cold_normal_writes_keep_historical_predecessors_cold() {
        let (store, _) = fixture("historical-chain");
        let mut raw = store.load_raw_json_value().unwrap();
        raw["sessions"][0]["completion_status"] = Value::Null;
        raw["sessions"][0]["stopped_at"] = Value::Null;
        raw["sessions"][0]["status"] = json!("idle");
        raw["sessions"][0]["predecessor_session_id"] = json!("X");
        raw["sessions"][1]["predecessor_session_id"] = json!("Y");
        raw["sessions"][2]["predecessor_session_id"] = json!("Z");
        store.write_raw_json_value(&raw).unwrap();
        assert_eq!(store.sweep_cold_sessions().unwrap(), 2);
        store
            .set_agent_status(
                "A",
                AgentStatusRequest {
                    text: Some("working".into()),
                },
            )
            .unwrap();
        assert_eq!(store.load_cold_segment().unwrap().1.sessions.len(), 2);
        fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
    }

    #[test]
    fn cold_sweeps_move_oldest_thousand_and_preserve_null_anchors() {
        let (store, _) = fixture("limit");
        let records: Vec<_> = (0..1100).map(|i| json!({"id":format!("s{i:04}"),"status":"stopped","completion_status":"killed","last_activity":(OffsetDateTime::now_utc()-TimeDuration::days(15)-TimeDuration::seconds(i)).format(&Rfc3339).unwrap()})).collect();
        let raw = json!({"sessions":records});
        store.write_raw_json_value(&raw).unwrap();
        assert_eq!(store.sweep_cold_sessions().unwrap(), 1000);
        assert_eq!(
            store.load_raw_json_value().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .len(),
            100
        );
        assert_eq!(
            store.load_cold_segment().unwrap().1.sessions[0].record["id"],
            "s0100"
        );
        assert_eq!(*store.load_parsed_state().unwrap().raw, raw);
        assert_eq!(store.sweep_cold_sessions().unwrap(), 100);
        assert_eq!(
            store.load_cold_segment().unwrap().1.sessions[0].anchor,
            None
        );
        assert_eq!(*store.load_parsed_state().unwrap().raw, raw);
        assert_eq!(store.thaw_all_cold_sessions().unwrap(), 1100);
        assert_eq!(store.load_raw_json_value().unwrap(), raw);
        fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
    }

    #[test]
    fn cold_missing_anchor_and_duplicate_content_have_deterministic_positions() {
        let cold: ColdSegment = serde_json::from_value(json!({"version":1,"sessions":[
            {"anchor":null,"record":{"id":"first"}},
            {"anchor":"missing","record":{"id":"X","content":"cold"}},
            {"anchor":"A","record":{"id":"Y"}}],"session_runtime_launches":[]}))
        .unwrap();
        let hot = json!({"sessions":[{"id":"A"},{"id":"X","content":"hot"},{"id":"B"}]});
        let merged = cold.merge(&hot);
        assert_eq!(ids(&merged, "sessions"), ["first", "X", "A", "Y", "B"]);
        assert_eq!(merged["sessions"][1]["content"], "hot");
    }

    #[test]
    fn cold_excludes_active_reparent_participants_and_future_launch_states() {
        for status in ["pending", "applying", "failed"] {
            let (store, _) = fixture("reparent");
            let mut raw = store.load_raw_json_value().unwrap();
            raw["reparent_requests"] = json!([{"id":"request","kind":"reparent","subject_session_id":"X","target_parent_session_id":"Y","expected_parent_session_id":"Z","initiator_session_id":"A","status":status,"apply_stage":"routing_quiesced","created_at":"2025-01-01T00:00:00Z","expires_at":"2100-01-01T00:00:00Z","topology_fingerprint":"fixture"}]);
            store.write_raw_json_value(&raw).unwrap();
            assert_eq!(store.sweep_cold_sessions().unwrap(), 0, "{status}");
            raw.as_object_mut().unwrap().remove("reparent_requests");
            raw["session_runtime_launches"][1]["status"] = json!("future-unfinished-state");
            store.write_raw_json_value(&raw).unwrap();
            assert_eq!(store.sweep_cold_sessions().unwrap(), 2);
            assert!(store.load_raw_json_value().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["id"] == "X"));
            fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn cold_keeps_old_nonretired_codex_fork_attach_and_revival_unchanged() {
        let (hot, _) = fixture("attach-hot");
        let (cold, _) = fixture("attach-cold");
        let hot = hot.with_codex_fork_event_monitors_deferred();
        let cold = cold.with_codex_fork_event_monitors_deferred();
        let mut raw = hot.load_raw_json_value().unwrap();
        raw["sessions"][1]["completion_status"] = Value::Null;
        raw["sessions"][1]["provider"] = json!("codex-fork");
        raw["sessions"][1]["log_file"] = json!("/tmp/cold-attach-fixture.log");
        hot.write_raw_json_value(&raw).unwrap();
        cold.write_raw_json_value(&raw).unwrap();
        assert_eq!(cold.sweep_cold_sessions().unwrap(), 2);
        assert!(cold.load_raw_json_value().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == "X"));
        let runtime = TmuxRuntime::from_config(&crate::config::RustCoreConfig::default())
            .with_tmux_binary_for_test("/usr/bin/true");
        for store in [&hot, &cold] {
            assert_eq!(
                store
                    .revive_stopped_tmux_client_session("X", &runtime)
                    .unwrap()
                    .as_deref(),
                Some("X")
            );
            assert!(!store.get_session("X").unwrap().unwrap().is_stopped());
        }
        let mut a = (*hot.load_parsed_state().unwrap().raw).clone();
        let mut b = (*cold.load_parsed_state().unwrap().raw).clone();
        a["sessions"][1]["last_activity"] = Value::Null;
        b["sessions"][1]["last_activity"] = Value::Null;
        assert_eq!(a, b);
        fs::remove_dir_all(hot.state_file.parent().unwrap()).unwrap();
        fs::remove_dir_all(cold.state_file.parent().unwrap()).unwrap();
    }

    #[test]
    fn cold_sweep_and_by_id_mutation_serialize_without_lost_updates() {
        let (store, _) = fixture("race");
        let sweep = store.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let ready = barrier.clone();
        let writer = thread::spawn(move || {
            ready.wait();
            sweep.sweep_cold_sessions().unwrap();
        });
        barrier.wait();
        let result = store
            .set_agent_status(
                "X",
                AgentStatusRequest {
                    text: Some("updated".into()),
                },
            )
            .unwrap();
        assert!(result.is_some());
        writer.join().unwrap();
        assert_eq!(
            store
                .get_session("X")
                .unwrap()
                .unwrap()
                .agent_status_text
                .as_deref(),
            Some("updated")
        );
        assert_eq!(
            ids(&store.load_parsed_state().unwrap().raw, "sessions"),
            ["A", "X", "Y", "B", "Z"]
        );
        fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
    }

    #[test]
    fn cold_moves_preserve_order_at_all_sixteen_crash_points() {
        for stop in 1..=8 {
            let (store, before) = fixture("sweep-crash");
            let mut step = 0;
            let guard = store.write_guard().unwrap();
            assert!(store
                .sweep_cold_checkpoint(OffsetDateTime::now_utc(), &mut |_| {
                    step += 1;
                    if step == stop {
                        anyhow::bail!("crash");
                    }
                    Ok(())
                })
                .is_err());
            drop(guard);
            assert_restart(&store, &before);
            store.sweep_cold_sessions().unwrap();
            assert_restart(&store, &before);
            fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
        }
        for stop in 1..=8 {
            let (store, before) = fixture("thaw-crash");
            store.sweep_cold_sessions().unwrap();
            let guard = store.write_guard().unwrap();
            let mut step = 0;
            assert!(store
                .thaw_locked_checkpoint(|r| r["id"] == "X", &mut |_| {
                    step += 1;
                    if step == stop {
                        anyhow::bail!("crash");
                    }
                    Ok(())
                })
                .is_err());
            drop(guard);
            assert_restart(&store, &before);
            store.sweep_cold_sessions().unwrap();
            assert_restart(&store, &before);
            fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn cold_hot_content_wins_at_cold_position_and_thaw_reanchors() {
        let (store, before) = fixture("duplicate");
        store.sweep_cold_sessions().unwrap();
        let guard = store.write_guard().unwrap();
        store.thaw_locked(&guard, |r| r["id"] == "X").unwrap();
        assert_eq!(
            store.load_cold_segment().unwrap().1.sessions[0]
                .anchor
                .as_deref(),
            Some("X")
        );
        assert_eq!(
            ids(&store.load_raw_json_value().unwrap(), "sessions"),
            ["A", "X", "B"]
        );
        drop(guard);
        assert_restart(&store, &before);
        assert_eq!(store.thaw_all_cold_sessions().unwrap(), 2);
        assert_eq!(*store.load_parsed_state().unwrap().raw, before);
        assert!(!store.cold_path().exists());
        fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
    }

    #[test]
    fn cold_eligibility_is_conservative_and_uses_first_parseable_stop_time() {
        let (store, _) = fixture("eligibility");
        let mut raw = store.load_raw_json_value().unwrap();
        let mut records = Vec::new();
        for i in 0..12 {
            let mut record = raw["sessions"][1].clone();
            record["id"] = json!(format!("s{i}"));
            records.push(record);
        }
        records[0]["completion_status"] = Value::Null;
        records[1]["stopped_at"] = json!(OffsetDateTime::now_utc().format(&Rfc3339).unwrap());
        records[2]["is_em"] = json!(true);
        records[8]["stopped_at"] = json!("bad"); // Falls back to last_activity.
        records[9]["stopped_at"] = Value::Null;
        records[9]["last_activity"] = Value::Null;
        records[10]["status"] = json!("running");
        records[10]["stopped_at"] = Value::Null;
        records[10]["parent_session_id"] = json!("s7");
        records[10]["completion_status"] = Value::Null;
        raw["sessions"] = json!(records);
        raw["agent_registrations"] = json!([{"session_id":"s3"}]);
        raw["maintainer_session_id"] = json!("s4");
        raw["session_runtime_launches"] =
            json!([{"id":"L","session_id":"s5","status":"teardown_pending"}]);
        raw["handoff_deferred_sessions"] = json!({"s6":false});
        store.write_raw_json_value(&raw).unwrap();
        assert_eq!(store.sweep_cold_sessions().unwrap(), 2);
        assert_eq!(
            store
                .load_cold_segment()
                .unwrap()
                .1
                .sessions
                .iter()
                .map(|e| e.record["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["s8", "s11"]
        );
        fs::remove_dir_all(store.state_file.parent().unwrap()).unwrap();
    }
}
