//! Questions beside a published doc (sm#1840, appendix D of the 1821 memo).
use super::*;
use crate::owner_docs::{OwnerDoc, OwnerDocPublish};
use crate::owner_inbox::OwnerNote;

#[derive(Deserialize)]
struct AskRequest {
    text: String,
    #[serde(default)]
    quote: Option<String>,
    target: String,
}

fn bad(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    }
}

fn conflict(detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status: StatusCode::CONFLICT,
        detail: detail.into(),
    }
}

fn latest_publish(state: &AppState, doc: &OwnerDoc) -> Result<OwnerDocPublish, ApiError> {
    docs::owner_doc_store(state)
        .publishes(&doc.id)?
        .pop()
        .ok_or(ApiError::NotFound("Doc has no published revision"))
}

fn reader_session(state: &AppState, doc_id: &str) -> Result<Option<SessionRecord>, ApiError> {
    let Some((id, _)) = docs::owner_doc_store(state).reader(doc_id)? else {
        return Ok(None);
    };
    Ok(state.session_store.get_session(&id)?)
}

fn author_session(state: &AppState, doc: &OwnerDoc) -> Result<Option<SessionRecord>, ApiError> {
    Ok(state.session_store.get_session(&doc.author_session_id)?)
}

fn choose_default(author_live: bool) -> &'static str {
    if author_live {
        "author"
    } else {
        "reader"
    }
}

pub(super) async fn target(
    state: Arc<AppState>,
    doc_id: String,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let doc = docs::find_doc(&state, &doc_id)?;
    let author = author_session(&state, &doc)?;
    let reader = reader_session(&state, &doc_id)?;
    let (thread_key, first_published_at) = inbox::work_threads::ThreadCatalog::load(&state)?
        .doc_key(&doc_id)
        .ok_or(ApiError::NotFound("Doc thread not found"))?;
    Ok(Json(json!({
        "author": author.as_ref().map(|session| json!({
            "id": session.id, "name": session_display_name(session.clone()),
            "state": if session.is_stopped() { "ended" } else { "live" },
            "live": !session.is_stopped(),
            "restorable": session.is_stopped() &&
                is_primary_node(&session.node) &&
                matches!(session.provider.as_str(), "claude" | "codex" | "codex-fork"),
            "context_tokens": session.context_total_input_tokens,
        })),
        "default": choose_default(author.as_ref().is_some_and(|s| !s.is_stopped())),
        "reader": reader.as_ref().filter(|s| !s.is_stopped())
            .map(|s| json!({"id": s.id, "name": session_display_name(s.clone())})),
        "thread_key": thread_key,
        "first_published_at": first_published_at,
    }))
    .into_response())
}

fn reader_worktree(
    state: &AppState,
    doc: &OwnerDoc,
    publish: &OwnerDocPublish,
) -> Result<String, ApiError> {
    let root = publish
        .checkout_root
        .as_deref()
        .map(expand_home)
        .filter(|path| path.is_dir())
        .or_else(|| {
            author_session(state, doc)
                .ok()
                .flatten()
                .map(|session| expand_home(&session.working_dir))
                .filter(|path| path.is_dir())
        })
        .ok_or_else(|| conflict("No local checkout is available for this doc"))?;
    let repo = doc.repo.rsplit('/').next().unwrap_or("repo");
    let path = state
        .config
        .work_claims
        .worktree_root_path()
        .join(format!("{repo}-ask-{}", doc.id));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    checkout_reader_worktree(&root, &path, &publish.commit_sha)?;
    Ok(path.display().to_string())
}

fn checkout_reader_worktree(root: &StdPath, path: &StdPath, sha: &str) -> Result<(), ApiError> {
    if path.exists() {
        return Err(conflict(
            "The reader worktree already exists; retire its previous reader first",
        ));
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "add", "--detach"])
        .arg(path)
        .arg(sha)
        .output()?;
    if !output.status.success() {
        return Err(conflict(format!(
            "Cannot check out doc commit: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn question_text(
    owner: &str,
    title: &str,
    sha: &str,
    text: &str,
    quote: Option<&str>,
) -> (String, String) {
    let short = &sha[..sha.len().min(7)];
    let question = quote.map_or_else(
        || text.to_owned(),
        |q| {
            let quoted = q
                .lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{quoted}\n\n{text}")
        },
    );
    let delivered = format!(
        "[Question from {owner} about \"{title}\" rev {short}]\n\n{question}\n\nAnswer with sm send rajesh."
    );
    (question, delivered)
}

/// A reader has no ticket claim, so ordinary claim-worktree cleanup does not
/// see its detached checkout. Preserve it if the reader changed files.
pub(super) fn cleanup_reader_worktree(state: &AppState, session_id: &str) -> anyhow::Result<()> {
    let Some(path) = docs::owner_doc_store(state).reader_worktree_for_session(session_id)? else {
        return Ok(());
    };
    if !StdPath::new(&path).exists() {
        return Ok(());
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["worktree", "remove", &path])
        .output()?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

async fn start_reader(state: &Arc<AppState>, doc: &OwnerDoc) -> Result<SessionRecord, ApiError> {
    let publish = latest_publish(state, doc)?;
    let work = state.clone();
    let d = doc.clone();
    let p = publish.clone();
    let working_dir = tokio::task::spawn_blocking(move || reader_worktree(&work, &d, &p))
        .await
        .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??;
    let short = &publish.commit_sha[..publish.commit_sha.len().min(7)];
    let brief = format!(
        "You are a reader agent for {repo}/{path} at commit {sha}. PR: {pr}. \
         Answer the owner's questions about this doc with sm send rajesh. Read the doc, its code and PR \
         before answering. Do not edit files, commit, push, comment on GitHub, or message other agents. \
         After each answer run sm task-complete. Stay available for later questions about this doc.",
        repo = doc.repo, path = doc.path, sha = publish.commit_sha,
        pr = doc.pr_number.map_or_else(|| "none".to_owned(), |n| format!("https://github.com/{}/pull/{n}", doc.repo)),
    );
    let request = CreateCoreSessionRequest {
        id: None,
        name: Some(format!(
            "ask-{}-{short}",
            doc.repo.rsplit('/').next().unwrap_or("doc")
        )),
        working_dir: Some(working_dir.clone()),
        provider: Some("claude".into()),
        parent_session_id: None,
        node: None,
        initial_message: Some(brief),
        model: Some("sonnet".into()),
        reasoning_effort: Some("high".into()),
        wait: None,
        spawn_prompt_source: None,
        spawn_brief: None,
        started_by_sm: true,
    };
    let session = match create_session_from_request(state.clone(), request).await {
        Ok(session) => session,
        Err(error) => {
            let path = working_dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                Command::new("git")
                    .arg("-C")
                    .arg(&path)
                    .args(["worktree", "remove", "--force", &path])
                    .status()
            })
            .await;
            return Err(error);
        }
    };
    docs::owner_doc_store(state).set_reader(&doc.id, &session.id, &working_dir)?;
    Ok(session)
}

pub(super) async fn send(
    state: Arc<AppState>,
    doc_id: String,
    peer_addr: SocketAddr,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    if !docs::doc_token_presented(&state, &headers, &doc_id) {
        ensure_session_allowed_from_parts(
            &state.config,
            &headers,
            Some(peer_addr),
            &format!("/docs/{doc_id}/ask"),
        )?;
    }
    ensure_core_writes_enabled(&state)?;
    let payload: AskRequest = docs::parse_json_body(&body)?;
    let text = payload.text.trim();
    docs::validated_draft_body(text)?;
    if text.is_empty() {
        return Err(bad("Question is empty"));
    }
    let quote = payload
        .quote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if quote.is_some_and(|s| s.chars().count() > docs::MAX_DRAFT_QUOTE) {
        return Err(bad("Quote is too long"));
    }
    let doc = docs::find_doc(&state, &doc_id)?;
    let publish = latest_publish(&state, &doc)?;
    let (thread_key, _) = inbox::work_threads::ThreadCatalog::load(&state)?
        .doc_key(&doc_id)
        .ok_or(ApiError::NotFound("Doc thread not found"))?;
    let _guard = state.owner_message_lock.lock().await;
    let author = author_session(&state, &doc)?;
    let current_reader = reader_session(&state, &doc_id)?;
    let recipient = match payload.target.as_str() {
        "author" => author
            .filter(|s| !s.is_stopped())
            .ok_or_else(|| conflict("The author has ended; select Reader or Bring back author"))?,
        "restore_author" => {
            let author = author.ok_or_else(|| conflict("This doc has no known author"))?;
            if !author.is_stopped() {
                author
            } else {
                let work = state.clone();
                let id = author.id.clone();
                tokio::task::spawn_blocking(move || {
                    auto_retire::restore_session_with_work(&work, &id)
                })
                .await
                .map_err(|error| ApiError::from(anyhow::anyhow!(error)))??
            }
        }
        "reader" => match current_reader {
            Some(reader) if !reader.is_stopped() => reader,
            Some(reader) => {
                let work = state.clone();
                let id = reader.id.clone();
                tokio::task::spawn_blocking(move || cleanup_reader_worktree(&work, &id))
                    .await
                    .map_err(|error| ApiError::from(anyhow::anyhow!(error)))?
                    .map_err(|error| {
                        conflict(format!(
                            "Cannot retire the previous reader worktree: {error}"
                        ))
                    })?;
                start_reader(&state, &doc).await?
            }
            _ => start_reader(&state, &doc).await?,
        },
        _ => return Err(bad("target must be author, reader or restore_author")),
    };
    let (question, delivered_text) = question_text(
        &state.config.owner_name,
        &doc.title,
        &publish.commit_sha,
        text,
        quote,
    );
    let id = format!("ask-{}", state.session_store.allocate_session_id()?);
    let (note, inserted) = inbox::inbox_store(&state).record_note(&OwnerNote {
        id,
        session_id: recipient.id.clone(),
        body: question,
        delivered_text,
        delivered_to_session_id: recipient.id.clone(),
        created_at: String::new(),
        thread_key: Some(thread_key.clone()),
    })?;
    if inserted {
        messages::deliver_now(&state, &recipient.id, &note.id);
    }
    Ok(Json(json!({"id": note.id, "thread_key": thread_key,
        "recipient": {"id": recipient.id, "name": session_display_name(recipient)}}))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_author_takes_priority_and_ended_author_does_not() {
        assert_eq!(choose_default(false), "reader");
        assert_eq!(choose_default(true), "author");
    }

    #[test]
    fn quote_and_revision_reach_the_agent_in_one_message() {
        let (display, delivered) = question_text(
            "Owner",
            "Risk memo",
            "abcdef012345",
            "Why 60 minutes?",
            Some("first line\nsecond line"),
        );
        assert_eq!(display, "> first line\n> second line\n\nWhy 60 minutes?");
        assert_eq!(delivered, "[Question from Owner about \"Risk memo\" rev abcdef0]\n\n> first line\n> second line\n\nWhy 60 minutes?\n\nAnswer with sm send rajesh.");
    }

    #[test]
    fn reader_checks_out_the_published_commit_detached() {
        let base = std::env::temp_dir().join(format!(
            "sm-1840-{}-{}",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let root = base.join("repo");
        let reader = base.join("reader");
        fs::create_dir_all(&root).unwrap();
        let git = |dir: &StdPath, args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        fs::write(root.join("memo.html"), "first").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "first"]);
        let pinned = git(&root, &["rev-parse", "HEAD"]);
        fs::write(root.join("memo.html"), "second").unwrap();
        git(&root, &["commit", "-qam", "second"]);
        checkout_reader_worktree(&root, &reader, &pinned).unwrap();
        assert_eq!(git(&reader, &["rev-parse", "HEAD"]), pinned);
        assert_eq!(git(&reader, &["rev-parse", "--abbrev-ref", "HEAD"]), "HEAD");
        assert_eq!(
            fs::read_to_string(reader.join("memo.html")).unwrap(),
            "first"
        );
        git(&root, &["worktree", "remove", reader.to_str().unwrap()]);
        fs::remove_dir_all(base).unwrap();
    }
}
