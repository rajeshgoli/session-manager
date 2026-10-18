//! Device-only backup transport. No local bypass applies to these routes.
use super::*;
use crate::managed_artifacts::{
    backup::{self as bk, lifecycle as life},
    ManagedArtifacts,
};

fn error(status: StatusCode, detail: impl Into<String>) -> ApiError {
    ApiError::Status {
        status,
        detail: detail.into(),
    }
}
fn refusal(e: anyhow::Error) -> ApiError {
    let text = e.to_string();
    error(
        if text == "backup request not found" {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::CONFLICT
        },
        text,
    )
}
fn service(state: &AppState) -> Result<ManagedArtifacts, ApiError> {
    state
        .managed_artifacts
        .as_ref()
        .filter(|m| m.config.backup.enabled)
        .cloned()
        .ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "backup is disabled"))
}
#[derive(Clone)]
struct Device(String);

/// On every listener, a verified backup identity may access only backup routes.
/// A backup route additionally requires the mobile hostname/audience and an
/// explicitly allowed common name, even on loopback or with an owner cookie.
pub(super) async fn guard(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path();
    let backup_route = path.starts_with("/backup/") && path != "/backup/allow-deletes";
    let result = (|| -> Result<Option<String>, ApiError> {
        let application = request_cloudflare_access_application(&state, &request);
        let assertion = header_text(request.headers(), "cf-access-jwt-assertion");
        // Unverified parsing is only a deny-path hint, never authorization.
        // Leave unrelated identities to their existing route guards.
        let claimed_puller = assertion
            .as_deref()
            .and_then(|a| a.split('.').nth(1))
            .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["common_name"].as_str().map(str::to_owned))
            .is_some_and(|n| {
                state
                    .config
                    .managed_artifacts
                    .backup
                    .puller_devices
                    .contains(&n)
            });
        if !backup_route {
            return if claimed_puller {
                Err(error(
                    StatusCode::FORBIDDEN,
                    "backup devices cannot access this route",
                ))
            } else {
                Ok(None)
            };
        }
        let identity = match (application, assertion) {
            (Some(app), Some(assertion)) => Some(
                classify_cloudflare_access_assertion_cached(&state, app, &assertion)
                    .map_err(cloudflare_access_error)?,
            ),
            _ => None,
        };
        let name = identity.as_ref().and_then(|c| c.common_name.as_deref());
        let puller = name.is_some_and(|n| {
            state
                .config
                .managed_artifacts
                .backup
                .puller_devices
                .iter()
                .any(|d| d == n)
        });
        if backup_route {
            if application != Some(CloudflareAccessApplication::MobileApp) || !puller {
                return Err(error(
                    StatusCode::FORBIDDEN,
                    "backup requires an allowed device certificate on the mobile hostname",
                ));
            }
            service(&state)?;
            Ok(name.map(str::to_owned))
        } else if puller {
            Err(error(
                StatusCode::FORBIDDEN,
                "backup devices cannot access this route",
            ))
        } else {
            Ok(None)
        }
    })();
    match result {
        Ok(Some(name)) => {
            request.extensions_mut().insert(Device(name));
        }
        Ok(None) => {}
        Err(e) => return e.into_response(),
    }
    next.run(request).await
}
fn device(request: &Request) -> Result<String, ApiError> {
    request
        .extensions()
        .get::<Device>()
        .map(|d| d.0.clone())
        .ok_or_else(|| error(StatusCode::FORBIDDEN, "backup device identity missing"))
}
#[derive(Deserialize)]
struct Renewal {
    csr_pem: String,
    certificate_pem: String,
}
pub(super) async fn renew(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let name = device(&request)?;
    let Json(body) = Json::<Renewal>::from_request(request, &state)
        .await
        .map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
    if body.csr_pem.len() > 16_384 || body.certificate_pem.len() > 32_768 {
        return Err(error(StatusCode::BAD_REQUEST, "renewal input too large"));
    }
    let config = state.config.clone();
    let chain = tokio::task::spawn_blocking(move || {
        mobile_devices::renew_backup(&config, &name, &body.csr_pem, &body.certificate_pem)
    })
    .await
    .map_err(|e| ApiError::from(anyhow::anyhow!(e)))?
    .map_err(refusal)?;
    Ok(Json(json!({"certificate_chain_pem":chain})))
}
#[derive(Deserialize)]
struct Have {
    have: Option<String>,
}
pub(super) async fn request(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let device = device(&request)?;
    let manager = service(&state)?;
    let Json(body) = Json::<Have>::from_request(request, &state)
        .await
        .map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
    tokio::task::spawn_blocking(move || {
        let result = life::request(&manager, &device, body.have.as_deref()).map_err(refusal)?;
        match result {
            life::RequestResult::Mismatch(expected) => {
                Ok((StatusCode::CONFLICT, Json(json!({"expected":expected}))).into_response())
            }
            life::RequestResult::Open(status) => {
                life::schedule(
                    &manager,
                    &expand_home(&state.config.queue_runner_state_dir().to_string_lossy()),
                    &std::env::current_exe()?,
                )?;
                Ok((StatusCode::ACCEPTED, Json(status)).into_response())
            }
        }
    })
    .await?
}
pub(super) async fn status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let device = device(&request)?;
    let manager = service(&state)?;
    tokio::task::spawn_blocking(move || {
        Ok(Json(life::status(&manager, &device, &id).map_err(refusal)?).into_response())
    })
    .await?
}
pub(super) async fn ack(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let device = device(&request)?;
    let manager = service(&state)?;
    let Json(body) = Json::<life::Ack>::from_request(request, &state)
        .await
        .map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
    tokio::task::spawn_blocking(move || {
        life::acknowledge(&manager, &device, &id, &body).map_err(refusal)?;
        deliver_alerts(&state, &manager)?;
        Ok(Json(json!({"ok":true})).into_response())
    })
    .await?
}
fn byte_range(value: Option<&str>, size: u64) -> Result<(u64, u64, bool), ApiError> {
    let bad = || {
        error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "invalid backup byte range",
        )
    };
    let Some(value) = value else {
        return Ok((0, size, false));
    };
    let (start, end) = value
        .strip_prefix("bytes=")
        .and_then(|v| v.split_once('-'))
        .ok_or_else(bad)?;
    if size == 0 {
        return Err(bad());
    }
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| bad())?;
        if suffix == 0 {
            return Err(bad());
        }
        let length = suffix.min(size);
        return Ok((size - length, length, true));
    }
    let start = start.parse::<u64>().map_err(|_| bad())?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().map_err(|_| bad())?.min(size - 1)
    };
    if start >= size || start > end {
        return Err(bad());
    }
    Ok((start, end - start + 1, true))
}
pub(super) async fn part(
    State(state): State<Arc<AppState>>,
    Path((id, n)): Path<(String, usize)>,
    request: Request,
) -> Result<Response, ApiError> {
    let device = device(&request)?;
    let manager = service(&state)?;
    let range = header_text(request.headers(), "range");
    let (file, bytes) = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        let status = life::status(&manager, &device, &id).map_err(refusal)?;
        if status.state != "ready" {
            return Err(error(StatusCode::CONFLICT, "backup request is not ready"));
        }
        let part = status
            .parts
            .iter()
            .find(|p| p.n == n)
            .ok_or_else(|| error(StatusCode::NOT_FOUND, "backup part not found"))?;
        let directory: String = manager.store.open()?.query_row(
            "SELECT outbox FROM backup_requests WHERE id=?1 AND device=?2",
            rusqlite::params![id, device],
            |row| row.get(0),
        )?;
        let file = fs::File::open(PathBuf::from(directory).join(&part.file))?;
        Ok((file, part.bytes))
    })
    .await??;
    ranged(file, bytes, range.as_deref(), "application/zstd").await
}
/// Streams `file` (of `bytes` bytes), honouring a single `Range` header.
async fn ranged(
    file: fs::File,
    bytes: u64,
    range: Option<&str>,
    content_type: &'static str,
) -> Result<Response, ApiError> {
    let (start, length, partial) = byte_range(range, bytes)?;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::from_std(file);
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let body = Body::from_stream(stream::try_unfold(
        (file, length),
        |(mut file, left)| async move {
            if left == 0 {
                return Ok::<_, std::io::Error>(None);
            }
            let mut buffer = vec![0u8; left.min(128 * 1024) as usize];
            let n = file.read(&mut buffer).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated backup part",
                ));
            }
            buffer.truncate(n);
            Ok(Some((Bytes::from(buffer), (file, left - n as u64))))
        },
    ));
    let mut response = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header("accept-ranges", "bytes")
        .header("content-length", length)
        .header(CONTENT_TYPE, content_type);
    if partial {
        response = response.header(
            "content-range",
            format!("bytes {start}-{}/{bytes}", start + length - 1),
        );
    }
    response
        .body(body)
        .map_err(|e| ApiError::from(anyhow::anyhow!(e)))
}
fn published_cli(state: &AppState) -> Result<(bk::update::CliInfo, PathBuf), ApiError> {
    let dir = expand_home(&state.config.managed_artifacts.backup.cli_dir);
    let info = bk::update::published(&dir)?.ok_or_else(|| {
        error(
            StatusCode::NOT_FOUND,
            "no sm is published for the backup mini",
        )
    })?;
    Ok((info, dir.join(bk::update::CLI_FILE)))
}
/// `{version, sha256, bytes}` of the `sm` the mini should run (#2264). Asking
/// also starts a build of this server's commit when that is not what is
/// published; the mini picks it up on a later night.
pub(super) async fn cli(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    device(&request)?;
    service(&state)?;
    tokio::task::spawn_blocking(move || {
        if let Some(commit) = bk::cli_build::server_commit() {
            if let Err(error) = request_cli_build(&state, commit) {
                eprintln!("backup mini sm build was not queued: {error:#}");
            }
        }
        Ok(Json(published_cli(&state)?.0).into_response())
    })
    .await?
}
fn request_cli_build(state: &AppState, commit: &str) -> anyhow::Result<()> {
    let backup = &state.config.managed_artifacts.backup;
    bk::cli_build::request(
        &bk::cli_build::Build {
            dir: &expand_home(&backup.cli_dir),
            config: &backup.cli_build,
            queue_dir: &expand_home(&state.config.queue_runner_state_dir().to_string_lossy()),
            executable: &std::env::current_exe()?,
            commit,
            version: bk::update::CLI_VERSION,
        },
        OffsetDateTime::now_utc(),
    )?;
    Ok(())
}
/// One ticket per commit whose build failed. The mini keeps its current sm.
fn file_cli_build_failure(
    state: &AppState,
    manager: &ManagedArtifacts,
    commit: &str,
) -> anyhow::Result<()> {
    let queue_dir = expand_home(&state.config.queue_runner_state_dir().to_string_lossy());
    let Some(failure) = bk::cli_build::failure(&queue_dir, commit)? else {
        return Ok(());
    };
    let conn = manager.store.open()?;
    let key = format!("backup_cli_ticket:{commit}");
    if crate::managed_artifacts::store::meta(&conn, &key)?.is_some() {
        return Ok(());
    }
    let config = &state.config.managed_artifacts.backup.cli_build;
    let mut tail = failure.log_tail.replace("~~~", "~ ~ ~");
    if tail.len() > 20_000 {
        let mut start = tail.len() - 20_000;
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail = tail[start..].to_owned();
    }
    let title = format!(
        "Auto-filed: backup mini sm build failed at {}",
        &commit[..12]
    );
    let body = format!(
        "sm-server at commit `{commit}` could not build the `sm` that the remote backup Mac mini \
         runs (x86_64, macOS 10.13; #2264). Nothing changed on the mini: it keeps its current \
         `sm`. sm builds again when the mini next asks after {} hours, and on the first request \
         after any deploy of another commit.\n\n\
         To reproduce, from a checkout of `{commit}`: \
         `SM_BUILD_SHA={commit} scripts/build-backup-cli.sh stage --target-dir target \
         --expect-version '{}' --dir \"$(mktemp -d)\"`.\n\n\
         Queue job `{}` ended `{}`. End of its log:\n\n~~~\n{tail}\n~~~\n",
        config.retry_hours,
        bk::update::CLI_VERSION,
        failure.job_id,
        failure.state,
    );
    let (number, url) = state
        .board_source
        .create_issue(&config.ticket_repo, &title, &body)
        .map_err(|error| anyhow::anyhow!("file the backup mini build ticket: {error}"))?;
    crate::managed_artifacts::store::set_meta(&conn, &key, &url)?;
    if let Some(goal) = config.ticket_goal {
        let repo = crate::work_claims::canonical_repo(&config.ticket_repo);
        if let Err(error) =
            board::place_under(state, (repo.clone(), number), (repo, goal), "sm backup")
        {
            eprintln!("backup mini build ticket {url} was not placed under #{goal}: {error:#}");
        }
    }
    Ok(())
}
pub(super) async fn cli_binary(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    device(&request)?;
    service(&state)?;
    let range = header_text(request.headers(), "range");
    let (file, bytes) = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        let (info, path) = published_cli(&state)?;
        let file = fs::File::open(path)?;
        // The deploy renames the binary, then its description; between the two a
        // reader could pair them wrongly. The mini's SHA-256 check is the backstop.
        if file.metadata()?.len() != info.bytes {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "the published sm is being replaced; retry",
            ));
        }
        Ok((file, info.bytes))
    })
    .await??;
    ranged(file, bytes, range.as_deref(), "application/octet-stream").await
}
pub(super) async fn probe(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let device = device(&request)?;
    let manager = service(&state)?;
    let mut stream = request.into_body().into_data_stream();
    let mut bytes = 0u64;
    let mut start = None;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(120), stream.next())
        .await
        .map_err(|_| error(StatusCode::REQUEST_TIMEOUT, "backup probe timed out"))?
    {
        let chunk = chunk.map_err(|e| error(StatusCode::BAD_REQUEST, e.to_string()))?;
        start.get_or_insert_with(Instant::now);
        bytes += chunk.len() as u64;
        if bytes > 256 * 1024 * 1024 {
            return Err(error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "probe must be 256 MiB",
            ));
        }
    }
    if bytes != 256 * 1024 * 1024 {
        return Err(error(StatusCode::BAD_REQUEST, "probe must be 256 MiB"));
    }
    let seconds = start.unwrap().elapsed().as_secs_f64().max(f64::EPSILON);
    tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        bk::record_rate(&manager.store.open()?, &device, "probe", bytes, seconds)?;
        Ok(
            Json(json!({"bytes":bytes,"seconds":seconds,"bytes_per_second":bytes as f64/seconds}))
                .into_response(),
        )
    })
    .await?
}
pub(super) async fn summary(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let device = device(&request)?;
    let manager = service(&state)?;
    tokio::task::spawn_blocking(move ||->Result<_,ApiError>{let conn=manager.store.open()?;Ok(Json(json!({"device":device,"confirmed":bk::confirmed(&conn,&device)?.map(|m|m.manifest_id),"rates":bk::rates(&conn,&device,bk::parse_rate(&manager.config.backup.backup_rate_default)?)?})).into_response())}).await?
}
pub(super) async fn allow_deletes(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    if owner_web_guard(&state, &headers, Some(peer), "POST")?.is_none() {
        board::owner_guard(&state, &headers, peer, "POST", &uri, true)?;
    }
    let manager = service(&state)?;
    let device = body["device"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            (manager.config.backup.puller_devices.len() == 1)
                .then(|| manager.config.backup.puller_devices[0].clone())
        })
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "specify a backup device"))?;
    if !manager.config.backup.puller_devices.contains(&device) {
        return Err(error(StatusCode::BAD_REQUEST, "unknown backup device"));
    }
    tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        life::allow_deletes(&manager, &device)?;
        Ok(Json(json!({"ok":true})).into_response())
    })
    .await?
}
fn deliver_alerts(state: &AppState, manager: &ManagedArtifacts) -> anyhow::Result<()> {
    let conn = manager.store.open()?;
    for r in crate::managed_artifacts::store::query(
        &conn,
        "SELECT * FROM backup_alerts WHERE delivered=0",
        &[],
    )? {
        let key = r["key"].as_str().context("backup alert key")?;
        let result = messages::owner_message_store(state).create_once(
            crate::owner_messages::NewOwnerMessage {
                human: state.config.owner_name.clone(),
                sender_session_id: "sm-artifacts".into(),
                sender_session_name: "sm artifacts".into(),
                title: r["title"].as_str().unwrap_or("Backup").into(),
                body_markdown: r["body"].as_str().unwrap_or("").into(),
                blocking: r["blocking"] == 1,
            },
            Some(key),
        )?;
        if let crate::owner_messages::CreateOwnerMessage::Created(message) = result {
            follows::notice_new_message(state, &message);
            conn.execute("UPDATE backup_alerts SET delivered=1 WHERE key=?1", [key])?;
        }
    }
    Ok(())
}
impl AppState {
    pub fn backup_pass(&self) -> anyhow::Result<()> {
        let Some(manager) = self
            .managed_artifacts
            .as_ref()
            .filter(|m| m.config.backup.enabled)
        else {
            return Ok(());
        };
        life::expire(manager, OffsetDateTime::now_utc().unix_timestamp())?;
        life::schedule(
            manager,
            &expand_home(&self.config.queue_runner_state_dir().to_string_lossy()),
            &std::env::current_exe()?,
        )?;
        life::missed_alerts(manager, OffsetDateTime::now_utc().unix_timestamp())?;
        if let Some(commit) = bk::cli_build::server_commit() {
            if let Err(error) = file_cli_build_failure(self, manager, commit) {
                eprintln!("backup mini build failure was not filed: {error:#}");
            }
        }
        deliver_alerts(self, manager)
    }
}

#[cfg(test)]
mod tests;
