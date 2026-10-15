use super::*;
use crate::managed_artifacts::{
    self as ma, store as artifact_store, ManagedArtifacts, RecordFlags,
};

fn service(state: &AppState) -> Result<&ManagedArtifacts, ApiError> {
    state.managed_artifacts.as_ref().ok_or(ApiError::Status {
        status: StatusCode::CONFLICT,
        detail: "managed artifacts are disabled".into(),
    })
}
fn refusal(error: anyhow::Error) -> ApiError {
    let detail = error.to_string();
    let status = if detail.starts_with("artifact not found:") {
        StatusCode::NOT_FOUND
    } else if detail.contains("in use")
        || detail.contains("must be building")
        || detail.contains("already exists")
        || detail.contains("kept for")
        || detail.contains("input of")
        || detail.contains("requires your")
        || detail.contains("busy")
    {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    ApiError::Status { status, detail }
}
#[derive(Default, Deserialize)]
pub(super) struct ArtifactQuery {
    root: Option<String>,
    repo: Option<String>,
    state: Option<String>,
    epic: Option<String>,
}
pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ArtifactQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let svc = service(&state)?;
    let root = svc
        .config
        .resolve_root(q.root.as_deref(), q.repo.as_deref())
        .map_err(refusal)?;
    let mut artifacts = Vec::new();
    let mut epics: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for a in svc.store.list(&root.path)? {
        if q.state.as_ref().is_some_and(|s| a["state"] != s.as_str())
            || q.epic.as_ref().is_some_and(|e| a["epic"] != e.as_str())
        {
            continue;
        }
        let value = svc.store.detail(&root.path, a["name"].as_str().unwrap())?;
        if let Some(epic) = value["epic"].as_str() {
            let e = epics.entry(epic.into()).or_default();
            e.0 += value["disk_bytes"].as_u64().unwrap_or(0);
            e.1 += 1;
        }
        artifacts.push(value);
    }
    Ok(Json(json!({"artifacts":artifacts,"filesystems":filesystems(svc,root),"epics":epics.into_iter().map(|(label,(bytes,count))|json!({"label":label,"bytes":bytes,"count":count})).collect::<Vec<_>>(),"mode":svc.config.deletion_mode})).into_response())
}
pub(super) async fn show(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<ArtifactQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let svc = service(&state)?;
    let root = svc
        .config
        .resolve_root(q.root.as_deref(), q.repo.as_deref())
        .map_err(refusal)?;
    Ok(Json(svc.store.detail(&root.path, &name).map_err(refusal)?).into_response())
}
fn filesystems(svc: &ManagedArtifacts, root: &ma::config::ArtifactRoot) -> Vec<Value> {
    [(&root.path,"root"),(&root.cold_store,"cold")].into_iter().map(|(path,role)|{
        match nix::sys::statvfs::statvfs(StdPath::new(path)) {
            Ok(s)=>{let bytes=u64::from(s.blocks())*s.fragment_size();let floor=bytes*svc.config.floor_percent/100;json!({"path":path,"role":role,"bytes":bytes,"free":u64::from(s.blocks_available())*s.fragment_size(),"floor":floor,"enough":floor,"outstanding":0,"mounted":true,"waiting_jobs":[]})},
            Err(_)=>json!({"path":path,"role":role,"mounted":false,"bytes":null,"free":null,"floor":null,"enough":null,"outstanding":0,"waiting_jobs":[]}),
        }
    }).collect()
}
fn actor(state: &AppState, headers: &HeaderMap) -> Result<SessionRecord, ApiError> {
    let id = header_text(headers, "x-sm-session-id").ok_or(ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: "artifact commands require X-SM-Session-ID".into(),
    })?;
    let session = state
        .session_store
        .get_session(&id)?
        .ok_or(ApiError::NotFound("Session not found"))?;
    if messages::session_ended(&session) {
        return Err(ApiError::Status {
            status: StatusCode::CONFLICT,
            detail: "session has ended".into(),
        });
    }
    Ok(session)
}
pub(super) async fn create(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer), "/artifacts")?;
    tokio::task::spawn_blocking(move || {
        mutate(
            &state,
            &headers,
            body["name"].as_str().unwrap_or("").to_owned(),
            "new".into(),
            body,
        )
    })
    .await?
}
pub(super) async fn action(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((name, action)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    ensure_session_allowed_from_parts(&state.config, &headers, Some(peer), "/artifacts")?;
    tokio::task::spawn_blocking(move || mutate(&state, &headers, name, action, body)).await?
}
fn mutate(
    state: &AppState,
    headers: &HeaderMap,
    name: String,
    action: String,
    body: Value,
) -> Result<Response, ApiError> {
    let svc = service(state)?;
    let actor = actor(state, headers)?;
    let root = svc
        .config
        .resolve_root(body["root"].as_str(), body["repo"].as_str())
        .map_err(refusal)?;
    let result = (|| -> anyhow::Result<Value> {
        match action.as_str() {
            "new" | "record" => {
                let mut flags: RecordFlags = serde_json::from_value(body.clone())?;
                if flags.ticket.is_none() {
                    let claims =
                        claims::work_claim_store(state).claims_for_session(&actor.id, true)?;
                    let tickets: Vec<_> = claims
                        .iter()
                        .filter(|c| c.claim.repo == root.repo && c.claim.kind == "ticket")
                        .collect();
                    if tickets.len() == 1 {
                        flags.ticket = Some(tickets[0].claim.number);
                    }
                }
                if action == "new" {
                    svc.create(root, &name, &flags, &actor.id, &actor.name)
                } else {
                    let names = body
                        .get("names")
                        .map(|v| serde_json::from_value::<Vec<String>>(v.clone()))
                        .transpose()?
                        .unwrap_or_else(|| vec![name]);
                    svc.record(root, &names, &flags, &actor.id)
                }
            }
            "done" => svc.done(root, &name, &actor.id),
            "consumers" | "marks" => svc.edit(root, &name, &action, &body, &actor.id),
            "delete" => svc.delete(
                root,
                &name,
                &actor.id,
                body["reason"].as_str().unwrap_or(""),
            ),
            "unclaimable" | "report" => {
                let note = body["note"].as_str().unwrap_or("");
                ma::validate_note(note, svc.config.note_max_chars)?;
                let mut conn = svc.store.open()?;
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let artifact = artifact_store::artifact(&tx, &root.path, &name)?;
                if action == "unclaimable" {
                    let steward = state
                        .session_store
                        .lookup_agent_registration(&svc.config.steward_role)?;
                    if steward.is_none_or(|s| s.session_id != actor.id) {
                        anyhow::bail!("only the artifact steward may mark unclaimable");
                    }
                    if artifact["state"] != "unrecorded" {
                        anyhow::bail!("only unrecorded artifacts can be unclaimable");
                    }
                } else {
                    let investigation = artifact_store::meta(
                        &tx,
                        &format!("investigating:{}:{}", root.path, name.to_ascii_lowercase()),
                    )?;
                    if investigation.as_deref() != Some(&actor.id) {
                        anyhow::bail!("only the investigating session may report");
                    }
                    if !matches!(
                        body["recommend"].as_str(),
                        Some("record" | "keep" | "delete")
                    ) {
                        anyhow::bail!("recommend must be record, keep or delete");
                    }
                }
                artifact_store::event(&tx, &root.path, &name, &action, &actor.id, body.clone())?;
                let result = artifact_store::detail(&tx, &root.path, &name)?;
                tx.commit()?;
                Ok(result)
            }
            _ => anyhow::bail!("unknown artifact action: {action}"),
        }
    })()
    .map_err(refusal)?;
    Ok(Json(result).into_response())
}
impl AppState {
    pub fn managed_artifacts_pass(&self) -> anyhow::Result<()> {
        let Some(svc) = &self.managed_artifacts else {
            return Ok(());
        };
        let jobs = RetainedQueueStore::list_queue_jobs_from_path(
            &expand_home(&self.config.sm_send.db_path),
            Default::default(),
        )?;
        let alive = |id: &str| -> anyhow::Result<Option<SessionRecord>> {
            Ok(self.session_store.get_session(id)?.filter(|s| {
                !messages::session_ended(s)
                    && !matches!(s.status.as_str(), "stopped" | "killed" | "retired")
            }))
        };
        for root in &svc.config.roots {
            svc.scan(root)?;
            for a in svc.store.list(&root.path)? {
                let name = a["name"].as_str().unwrap();
                if a["state"] == "building" {
                    let owner = a["owner_session_id"]
                        .as_str()
                        .map(|id| self.session_store.get_session(id))
                        .transpose()?
                        .flatten();
                    let ended = owner.as_ref().is_none_or(messages::session_ended);
                    if ended && a["origin"] == "hand" && a["recorded_at"].is_string() {
                        let mut conn = svc.store.open()?;
                        let tx = conn
                            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                        if ma::ensure_not_in_use(&tx, &root.path, name).is_ok() {
                            tx.execute("UPDATE artifacts SET state='live' WHERE root=?1 AND name=?2 AND state='building'",rusqlite::params![root.path,name])?;
                            artifact_store::event(
                                &tx,
                                &root.path,
                                name,
                                "recorded",
                                "sm",
                                json!({"session_ended":true}),
                            )?;
                        }
                        tx.commit()?;
                    } else if ended {
                        // Refusals (age, use, retention) leave the build untouched.
                        let _ = svc.delete_orphan(root, name);
                    }
                }
                if a["state"] != "unrecorded" {
                    continue;
                }
                let mut conn = svc.store.open()?;
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let today = time::OffsetDateTime::now_utc().date().to_string();
                if !ma::attribution::request_due(&tx, &root.path, name, &today)? {
                    continue;
                }
                let reservations = artifact_store::query(
                    &tx,
                    "SELECT job_id,outputs_json FROM artifact_job_reservations WHERE root=?1",
                    &[&root.path],
                )?;
                let selected = ma::attribution::select(
                    || {
                        let mut owner = None;
                        for r in reservations {
                            let outputs: Vec<String> =
                                serde_json::from_str(r["outputs_json"].as_str().unwrap_or("[]"))?;
                            if outputs.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                                if let Some(job) = jobs.iter().find(|j| {
                                    Some(j.id.as_str()) == r["job_id"].as_str()
                                        && matches!(
                                            j.state.as_str(),
                                            "queued" | "running" | "holding"
                                        )
                                }) {
                                    if let Some(id) = &job.requester_session_id {
                                        owner = alive(id)?;
                                    }
                                }
                            }
                            if owner.is_some() {
                                break;
                            }
                        }

                        Ok(owner)
                    },
                    || {
                        if let Some(number) = ma::scan::ticket_prefix(name) {
                            if let Some(id) =
                                claims::work_claim_store(self).active_holder(&root.repo, number)?
                            {
                                return alive(&id);
                            }
                        }
                        Ok(None)
                    },
                    || {
                        if let Some(role) = self
                            .session_store
                            .lookup_agent_registration(&svc.config.steward_role)?
                        {
                            return alive(&role.session_id);
                        }
                        Ok(None)
                    },
                )?;
                let Some((owner, steward)) = selected else {
                    continue;
                };
                let text=format!("[sm artifact] {}/{name} appeared (size pending) with no record. Record it:\n  sm artifact record {name} --note \"one plain-English line\" --consumer '#N' [--recipe-complete] [--gpu-hours H --gpu-cost USD]\nUnrecorded folders are frozen on {}.{}",root.path,a["grace_until"].as_str().unwrap_or("unknown"),if steward{format!("\nRead its files, retention notes, git log -S {name} in the root's repo and the queue logs that mention it. Record it, or run sm artifact unclaimable {name} --note TEXT saying what you found.")}else{String::new()});
                let id = format!(
                    "artifact-record:{}:{}:{today}",
                    root.path,
                    name.to_ascii_lowercase()
                );
                RetainedQueueStore::new(expand_home(&self.config.sm_send.db_path))
                    .enqueue_message_once_with_metadata(
                        &id,
                        &owner.id,
                        &text,
                        "sequential",
                        crate::queue::QueueMessageMetadata {
                            message_category: Some("artifact".into()),
                            ..Default::default()
                        },
                    )?;
                tx.execute("UPDATE artifacts SET owner_session_id=?3,owner_session_name=?4 WHERE root=?1 AND name=?2",rusqlite::params![root.path,name,owner.id,owner.name])?;
                artifact_store::event(
                    &tx,
                    &root.path,
                    name,
                    "request_sent",
                    "sm",
                    json!({"session":owner.id}),
                )?;
                tx.commit()?;
            }
            let conn = svc.store.open()?;
            let key = format!("size_walk:{}", root.path);
            let previous = artifact_store::meta(&conn, &key)?.and_then(|s| {
                time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok()
            });
            let now = time::OffsetDateTime::now_utc();
            let perf = jobs.iter().any(|j| {
                j.job_type == "perf" && matches!(j.state.as_str(), "queued" | "running" | "holding")
            });
            if ma::attribution::size_walk_due(previous, now, &svc.config.nightly_time, perf)? {
                svc.measure_root(root)?;
                artifact_store::set_meta(&conn, &key, &ma::now())?;
            }
        }
        self.session_store
            .drain_runtime_pending_message_targets_by_category("artifact")?;
        Ok(())
    }
}
