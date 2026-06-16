#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::BTreeSet,
    fs,
    net::{SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use anyhow::{bail, Context, Result};
use axum::{
    extract::{ConnectInfo, Path as AxumPath, State},
    http::{header::HOST, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::Engine as _;
use p256::{ecdsa::VerifyingKey, pkcs8::DecodePublicKey};
use qrcode::{render::unicode, QrCode};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, sync::Notify};

use crate::{
    config::{trimmed, AppConfig, CloudflareAccessConfig},
    sessions::expand_home,
};

const EMPTY_DEVICE_COMMON_NAME: &str = "__sm_no_enrolled_mobile_devices__";
const MAX_UNKNOWN_PAIRING_ATTEMPTS: u32 = 25;
const PAIRING_PATH_PREFIX: &str = "/client/mobile-terminal/enroll";

#[derive(Debug, Clone)]
pub struct DeviceEnrollment {
    pub user_id: String,
    pub device_id: String,
    pub device_name: String,
    pub public_key_pem: String,
    pub common_name: String,
    pub paired_at: String,
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PairingRegistration {
    pub token: String,
    pub user_id: String,
    pub expires_at: String,
}

#[derive(Debug, Clone)]
pub struct EnrollDeviceOptions {
    pub config_path: PathBuf,
    pub user_id: String,
    pub expires_in_minutes: u64,
    pub listen: SocketAddr,
    pub advertised_base_url: Option<String>,
    pub device_ca_cert: Option<PathBuf>,
    pub device_ca_key: Option<PathBuf>,
    pub no_qr: bool,
}

#[derive(Debug, Clone)]
struct PairingState {
    config: AppConfig,
    db_path: PathBuf,
    ca_cert_path: PathBuf,
    ca_key_path: PathBuf,
    advertised_base_url: String,
    unknown_attempts: Arc<Mutex<u32>>,
    shutdown: Arc<Notify>,
}

#[derive(Debug, Deserialize)]
struct DeviceEnrollmentRequest {
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    device_name: String,
    #[serde(default)]
    csr_pem: String,
    #[serde(default)]
    public_key_pem: String,
}

#[derive(Debug, Serialize)]
struct DeviceEnrollmentResponse {
    device_id: String,
    device_name: String,
    certificate_chain_pem: String,
    expires_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePolicyAction {
    Allow,
    Revoke,
}

pub fn mobile_device_db_path(config: &AppConfig) -> PathBuf {
    expand_home(&config.mobile_terminal.device_enrollment_db_path)
}

pub fn active_device_public_key(
    db_path: &Path,
    user_id: &str,
    device_id: &str,
) -> Result<Option<String>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let connection = open_device_db(db_path)?;
    connection
        .query_row(
            r#"
            SELECT public_key_pem
            FROM mobile_device_enrollments
            WHERE user_id = ?
              AND device_id = ?
              AND revoked_at IS NULL
            LIMIT 1
            "#,
            params![user_id, device_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("failed to look up mobile device enrollment")
}

pub fn user_has_active_device(db_path: &Path, user_id: &str) -> Result<bool> {
    if !db_path.exists() {
        return Ok(false);
    }
    let connection = open_device_db(db_path)?;
    let count: i64 = connection
        .query_row(
            r#"
            SELECT COUNT(*)
            FROM mobile_device_enrollments
            WHERE user_id = ?
              AND revoked_at IS NULL
            "#,
            params![user_id],
            |row| row.get(0),
        )
        .context("failed to count mobile device enrollments")?;
    Ok(count > 0)
}

pub fn active_device_exists(db_path: &Path, user_id: &str, device_id: &str) -> Result<bool> {
    Ok(active_device_public_key(db_path, user_id, device_id)?.is_some())
}

pub fn device_exists(db_path: &Path, user_id: &str, device_id: &str) -> Result<bool> {
    if !db_path.exists() {
        return Ok(false);
    }
    let connection = open_device_db(db_path)?;
    let count: i64 = connection
        .query_row(
            r#"
            SELECT COUNT(*)
            FROM mobile_device_enrollments
            WHERE user_id = ?
              AND device_id = ?
            "#,
            params![user_id, device_id],
            |row| row.get(0),
        )
        .context("failed to count mobile device enrollment")?;
    Ok(count > 0)
}

pub fn list_active_devices_for_users(
    db_path: &Path,
    allowed_users: &BTreeSet<String>,
) -> Result<Vec<DeviceEnrollment>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let connection = open_device_db(db_path)?;
    let mut statement = connection
        .prepare(
            r#"
            SELECT user_id, device_id, device_name, public_key_pem, common_name, paired_at, revoked_at
            FROM mobile_device_enrollments
            WHERE revoked_at IS NULL
            ORDER BY user_id, device_id
            "#,
        )
        .context("failed to prepare mobile device list query")?;
    let rows = statement
        .query_map([], |row| {
            Ok(DeviceEnrollment {
                user_id: row.get(0)?,
                device_id: row.get(1)?,
                device_name: row.get(2)?,
                public_key_pem: row.get(3)?,
                common_name: row.get(4)?,
                paired_at: row.get(5)?,
                revoked_at: row.get(6)?,
            })
        })
        .context("failed to query mobile device enrollments")?;
    let mut devices = Vec::new();
    for row in rows {
        let device = row.context("failed to read mobile device enrollment row")?;
        if allowed_users.contains(&device.user_id) {
            devices.push(device);
        }
    }
    Ok(devices)
}

pub fn revoke_device(db_path: &Path, user_id: &str, device_id: &str) -> Result<bool> {
    if !db_path.exists() {
        return Ok(false);
    }
    let connection = open_device_db(db_path)?;
    let now = local_timestamp();
    let changed = connection
        .execute(
            r#"
            UPDATE mobile_device_enrollments
            SET revoked_at = ?
            WHERE user_id = ?
              AND device_id = ?
              AND revoked_at IS NULL
            "#,
            params![now, user_id, device_id],
        )
        .context("failed to revoke mobile device enrollment")?;
    if changed > 0 {
        insert_audit_event(
            &connection,
            Some(user_id),
            Some(device_id),
            "device_revoked",
            None,
            None,
        )?;
    }
    Ok(changed > 0)
}

pub fn create_pairing_registration(
    db_path: &Path,
    user_id: &str,
    expires_in_minutes: u64,
) -> Result<PairingRegistration> {
    let user_id = user_id.trim();
    if user_id.is_empty() {
        bail!("user_id must not be empty");
    }
    let ttl_minutes = expires_in_minutes.clamp(1, 60 * 24);
    let connection = open_device_db(db_path)?;
    let token = random_urlsafe_token(24);
    let now = local_timestamp();
    let expires_at = (time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        + time::Duration::minutes(ttl_minutes as i64))
    .format(&time::format_description::well_known::Rfc3339)
    .unwrap_or_else(|_| now.clone());
    connection
        .execute(
            r#"
            INSERT INTO mobile_device_pairings (token, user_id, created_at, expires_at)
            VALUES (?, ?, ?, ?)
            "#,
            params![token, user_id, now, expires_at],
        )
        .context("failed to create mobile device pairing registration")?;
    insert_audit_event(
        &connection,
        Some(user_id),
        None,
        "pairing_created",
        None,
        Some(&json!({ "expires_at": expires_at })),
    )?;
    Ok(PairingRegistration {
        token,
        user_id: user_id.to_owned(),
        expires_at,
    })
}

pub fn run_enroll_device(options: EnrollDeviceOptions) -> Result<()> {
    let config = AppConfig::load_from_path(&options.config_path)?;
    let Some(user_config) = config.mobile_terminal.allowed_users.get(&options.user_id) else {
        bail!(
            "mobile_terminal.allowed_users does not contain user_id {}",
            options.user_id
        );
    };
    if !user_config.interactive_shell_access {
        bail!(
            "mobile terminal user {} does not allow interactive shell access",
            options.user_id
        );
    }
    let db_path = mobile_device_db_path(&config);
    let ca_cert_path = options
        .device_ca_cert
        .clone()
        .unwrap_or_else(|| expand_home(&config.mobile_terminal.device_ca_cert_path));
    let ca_key_path = options
        .device_ca_key
        .clone()
        .unwrap_or_else(|| expand_home(&config.mobile_terminal.device_ca_key_path));
    ensure_device_ca(&ca_cert_path, &ca_key_path)?;
    ensure_cloudflare_mobile_device_ca(&config.cloudflare_access, &ca_cert_path)?;
    let registration = create_pairing_registration(
        &db_path,
        &options.user_id,
        if options.expires_in_minutes == 0 {
            config.mobile_terminal.device_enrollment_ttl_minutes
        } else {
            options.expires_in_minutes
        },
    )?;
    let base_url = options
        .advertised_base_url
        .clone()
        .unwrap_or_else(|| pairing_base_url(options.listen));
    let url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        enrollment_path(&registration.token).trim_start_matches('/')
    );
    println!("Device enrollment for user_id={}", registration.user_id);
    println!("Expires at: {}", registration.expires_at);
    println!("Enrollment URL: {url}");
    if !options.no_qr {
        println!();
        println!("{}", qr_ascii(&url)?);
    }
    println!(
        "Waiting for Android app enrollment on {} ...",
        options.listen
    );

    let runtime = tokio::runtime::Runtime::new().context("failed to start pairing runtime")?;
    runtime.block_on(serve_pairing_listener(
        config,
        db_path,
        options.listen,
        ca_cert_path,
        ca_key_path,
        base_url,
    ))
}

pub async fn serve_pairing_listener(
    config: AppConfig,
    db_path: PathBuf,
    bind_addr: SocketAddr,
    ca_cert_path: PathBuf,
    ca_key_path: PathBuf,
    advertised_base_url: String,
) -> Result<()> {
    let shutdown = Arc::new(Notify::new());
    let state = PairingState {
        config,
        db_path,
        ca_cert_path,
        ca_key_path,
        advertised_base_url,
        unknown_attempts: Arc::new(Mutex::new(0)),
        shutdown: shutdown.clone(),
    };
    let app = Router::new()
        .route("/health", get(pairing_health))
        .route(
            &format!("{PAIRING_PATH_PREFIX}/{{token}}"),
            get(open_device_enrollment).post(complete_device_enrollment),
        )
        .with_state(state);
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind mobile device pairing listener at {bind_addr}"))?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown.notified().await;
    })
    .await
    .context("mobile device pairing listener failed")
}

pub fn enrollment_path(token: &str) -> String {
    format!("{PAIRING_PATH_PREFIX}/{}", token.trim())
}

async fn pairing_health() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({"ok": true})))
}

async fn open_device_enrollment(
    State(state): State<PairingState>,
    AxumPath(token): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let token = token.trim().to_owned();
    if token.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Enrollment token is required");
    }
    match pending_pairing_registration(&state.db_path, &token) {
        Ok(Some(_registration)) => {}
        Ok(None) => {
            return json_error(
                StatusCode::NOT_FOUND,
                "Unknown, expired, or already used enrollment token",
            )
        }
        Err(error) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Enrollment lookup failed: {error}"),
            )
        }
    }
    let enrollment_url = format!(
        "{}/{}",
        request_base_url(&state, &headers).trim_end_matches('/'),
        enrollment_path(&token).trim_start_matches('/')
    );
    Html(enrollment_redirect_html(&enrollment_url)).into_response()
}

async fn complete_device_enrollment(
    State(state): State<PairingState>,
    AxumPath(token): AxumPath<String>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<DeviceEnrollmentRequest>,
) -> Response {
    let token = token.trim().to_owned();
    if token.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Enrollment token is required");
    }
    let device_id = payload.device_id.trim().to_owned();
    if !valid_device_id(&device_id) {
        return json_error(StatusCode::BAD_REQUEST, "Invalid device_id");
    }
    let device_name = payload
        .device_name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect::<String>();
    let device_name = if device_name.is_empty() {
        device_id.clone()
    } else {
        device_name
    };
    let registration = match pending_pairing_registration(&state.db_path, &token) {
        Ok(Some(registration)) => registration,
        Ok(None) => {
            let attempts = increment_unknown_attempts(&state);
            let _ = log_unknown_pairing_attempt(
                &state.db_path,
                &token,
                Some(&remote_addr.to_string()),
                if attempts > MAX_UNKNOWN_PAIRING_ATTEMPTS {
                    "too_many_unknown_attempts"
                } else {
                    "unknown_expired_or_already_paired"
                },
            );
            if attempts > MAX_UNKNOWN_PAIRING_ATTEMPTS {
                return json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many invalid enrollment attempts",
                );
            }
            return json_error(
                StatusCode::NOT_FOUND,
                "Unknown, expired, or already used enrollment token",
            );
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let Some(user_config) = state
        .config
        .mobile_terminal
        .allowed_users
        .get(&registration.user_id)
    else {
        return json_error(
            StatusCode::FORBIDDEN,
            "Enrollment user is no longer configured",
        );
    };
    if !user_config.interactive_shell_access {
        return json_error(
            StatusCode::FORBIDDEN,
            "Enrollment user is not allowed to use mobile terminal attach",
        );
    }
    let csr_public_key_pem = match extract_public_key_from_csr_with_openssl(&payload.csr_pem) {
        Ok(value) => value,
        Err(error) => {
            return invalid_csr_response(&state, &token, Some(&remote_addr.to_string()), &error)
        }
    };
    if let Err(error) = validate_cloudflare_client_csr_public_key(&csr_public_key_pem) {
        return invalid_csr_response(&state, &token, Some(&remote_addr.to_string()), &error);
    }
    if let Err(error) = validate_mobile_terminal_proof_public_key(&payload.public_key_pem) {
        return invalid_public_key_response(&state, &token, Some(&remote_addr.to_string()), &error);
    }
    let certificate_pem = match sign_csr_with_openssl(
        &state.ca_cert_path,
        &state.ca_key_path,
        &payload.csr_pem,
        &device_id,
    ) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let certificate_chain_pem =
        match build_certificate_chain_pem(&certificate_pem, &state.ca_cert_path) {
            Ok(value) => value,
            Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        };
    let completed_registration = match complete_pairing_registration(
        &state.db_path,
        &token,
        &device_id,
        &device_name,
        &payload.public_key_pem,
        Some(&remote_addr.to_string()),
    ) {
        Ok(Some(registration)) => registration,
        Ok(None) => {
            return json_error(
                StatusCode::NOT_FOUND,
                "Unknown, expired, or already used enrollment token",
            )
        }
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    if let Err(error) = sync_device_common_name(
        &state.config.cloudflare_access,
        &device_id,
        DevicePolicyAction::Allow,
    ) {
        let _ = revoke_device(&state.db_path, &completed_registration.user_id, &device_id);
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
    }
    state.shutdown.notify_waiters();
    Json(DeviceEnrollmentResponse {
        device_id,
        device_name,
        certificate_chain_pem,
        expires_at: completed_registration.expires_at,
    })
    .into_response()
}

fn json_error(status: StatusCode, detail: &str) -> Response {
    (status, Json(json!({ "detail": detail }))).into_response()
}

fn invalid_csr_response(
    state: &PairingState,
    token: &str,
    remote_addr: Option<&str>,
    error: &anyhow::Error,
) -> Response {
    let exhausted = record_pairing_failure(
        &state.db_path,
        token,
        "pairing_csr_rejected",
        remote_addr,
        &error.to_string(),
    )
    .unwrap_or(false);
    if exhausted {
        json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many invalid enrollment attempts",
        )
    } else {
        json_error(StatusCode::BAD_REQUEST, "Invalid device CSR")
    }
}

fn invalid_public_key_response(
    state: &PairingState,
    token: &str,
    remote_addr: Option<&str>,
    error: &anyhow::Error,
) -> Response {
    let exhausted = record_pairing_failure(
        &state.db_path,
        token,
        "pairing_public_key_rejected",
        remote_addr,
        &error.to_string(),
    )
    .unwrap_or(false);
    if exhausted {
        json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many invalid enrollment attempts",
        )
    } else {
        json_error(StatusCode::BAD_REQUEST, "Invalid device public key")
    }
}

fn increment_unknown_attempts(state: &PairingState) -> u32 {
    let mut attempts = state
        .unknown_attempts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *attempts += 1;
    *attempts
}

fn open_device_db(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create mobile device DB dir {}", parent.display())
        })?;
    }
    let connection = Connection::open(db_path)
        .with_context(|| format!("failed to open mobile device DB {}", db_path.display()))?;
    migrate_device_db(&connection)?;
    Ok(connection)
}

fn migrate_device_db(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS mobile_device_pairings (
                token TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                created_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                paired_at TEXT,
                failed_attempts INTEGER NOT NULL DEFAULT 0,
                last_failed_attempt_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_mobile_device_pairings_user_id ON mobile_device_pairings(user_id);
            CREATE INDEX IF NOT EXISTS idx_mobile_device_pairings_expires_at ON mobile_device_pairings(expires_at);

            CREATE TABLE IF NOT EXISTS mobile_device_enrollments (
                user_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                device_name TEXT NOT NULL,
                public_key_pem TEXT NOT NULL,
                common_name TEXT NOT NULL,
                paired_at TEXT NOT NULL,
                revoked_at TEXT,
                last_seen_at TEXT,
                PRIMARY KEY (user_id, device_id)
            );
            CREATE INDEX IF NOT EXISTS idx_mobile_device_enrollments_common_name ON mobile_device_enrollments(common_name);
            CREATE INDEX IF NOT EXISTS idx_mobile_device_enrollments_revoked_at ON mobile_device_enrollments(revoked_at);

            CREATE TABLE IF NOT EXISTS mobile_device_enrollment_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                user_id TEXT,
                device_id TEXT,
                event TEXT NOT NULL,
                remote_addr TEXT,
                details_json TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_mobile_device_enrollment_audit_timestamp ON mobile_device_enrollment_audit(timestamp);
            CREATE INDEX IF NOT EXISTS idx_mobile_device_enrollment_audit_device_id ON mobile_device_enrollment_audit(device_id);
            CREATE INDEX IF NOT EXISTS idx_mobile_device_enrollment_audit_event ON mobile_device_enrollment_audit(event);
            "#,
        )
        .context("failed to migrate mobile device enrollment DB")?;
    Ok(())
}

fn pending_pairing_registration(
    db_path: &Path,
    token: &str,
) -> Result<Option<PairingRegistration>> {
    let connection = open_device_db(db_path)?;
    let now = local_timestamp();
    connection
        .query_row(
            r#"
            SELECT token, user_id, expires_at
            FROM mobile_device_pairings
            WHERE token = ?
              AND paired_at IS NULL
              AND expires_at > ?
              AND failed_attempts < 5
            LIMIT 1
            "#,
            params![token, now],
            |row| {
                Ok(PairingRegistration {
                    token: row.get(0)?,
                    user_id: row.get(1)?,
                    expires_at: row.get(2)?,
                })
            },
        )
        .optional()
        .context("failed to look up mobile device pairing")
}

fn complete_pairing_registration(
    db_path: &Path,
    token: &str,
    device_id: &str,
    device_name: &str,
    public_key_pem: &str,
    remote_addr: Option<&str>,
) -> Result<Option<PairingRegistration>> {
    let mut connection = open_device_db(db_path)?;
    let transaction = connection
        .transaction()
        .context("failed to start mobile device enrollment transaction")?;
    let now = local_timestamp();
    let registration = transaction
        .query_row(
            r#"
            SELECT token, user_id, expires_at
            FROM mobile_device_pairings
            WHERE token = ?
              AND paired_at IS NULL
              AND expires_at > ?
              AND failed_attempts < 5
            LIMIT 1
            "#,
            params![token, now],
            |row| {
                Ok(PairingRegistration {
                    token: row.get(0)?,
                    user_id: row.get(1)?,
                    expires_at: row.get(2)?,
                })
            },
        )
        .optional()
        .context("failed to check pairing eligibility")?;
    let Some(registration) = registration else {
        return Ok(None);
    };
    transaction
        .execute(
            "UPDATE mobile_device_pairings SET paired_at = ? WHERE token = ?",
            params![now, token],
        )
        .context("failed to mark pairing used")?;
    transaction
        .execute(
            r#"
            INSERT INTO mobile_device_enrollments (
                user_id, device_id, device_name, public_key_pem, common_name, paired_at, revoked_at
            )
            VALUES (?, ?, ?, ?, ?, ?, NULL)
            ON CONFLICT(user_id, device_id) DO UPDATE SET
                device_name = excluded.device_name,
                public_key_pem = excluded.public_key_pem,
                common_name = excluded.common_name,
                paired_at = excluded.paired_at,
                revoked_at = NULL
            "#,
            params![
                registration.user_id,
                device_id,
                device_name,
                public_key_pem.trim(),
                device_id,
                now
            ],
        )
        .context("failed to upsert mobile device enrollment")?;
    insert_audit_event(
        &transaction,
        Some(&registration.user_id),
        Some(device_id),
        "pairing_completed",
        remote_addr,
        Some(&json!({
            "public_key_fingerprint": public_key_fingerprint(public_key_pem),
        })),
    )?;
    transaction
        .commit()
        .context("failed to commit mobile device enrollment")?;
    Ok(Some(registration))
}

fn record_pairing_failure(
    db_path: &Path,
    token: &str,
    event: &str,
    remote_addr: Option<&str>,
    error: &str,
) -> Result<bool> {
    let connection = open_device_db(db_path)?;
    let now = local_timestamp();
    let changed = connection
        .execute(
            r#"
            UPDATE mobile_device_pairings
            SET failed_attempts = failed_attempts + 1,
                last_failed_attempt_at = ?
            WHERE token = ?
              AND paired_at IS NULL
              AND expires_at > ?
            "#,
            params![now, token, now],
        )
        .context("failed to record mobile device pairing failure")?;
    let exhausted = if changed > 0 {
        let attempts: i64 = connection
            .query_row(
                "SELECT failed_attempts FROM mobile_device_pairings WHERE token = ?",
                params![token],
                |row| row.get(0),
            )
            .unwrap_or(0);
        attempts >= 5
    } else {
        false
    };
    insert_audit_event(
        &connection,
        None,
        None,
        event,
        remote_addr,
        Some(&json!({
            "token_hash": public_key_fingerprint(token),
            "error": error,
            "exhausted": exhausted,
        })),
    )?;
    Ok(exhausted)
}

fn log_unknown_pairing_attempt(
    db_path: &Path,
    token: &str,
    remote_addr: Option<&str>,
    event: &str,
) -> Result<()> {
    let connection = open_device_db(db_path)?;
    insert_audit_event(
        &connection,
        None,
        None,
        "pairing_token_rejected",
        remote_addr,
        Some(&json!({
            "token_hash": public_key_fingerprint(token),
            "reason": event,
        })),
    )
}

fn insert_audit_event(
    connection: &Connection,
    user_id: Option<&str>,
    device_id: Option<&str>,
    event: &str,
    remote_addr: Option<&str>,
    details: Option<&Value>,
) -> Result<()> {
    let details_json = details.map(Value::to_string);
    connection
        .execute(
            r#"
            INSERT INTO mobile_device_enrollment_audit (
                timestamp, user_id, device_id, event, remote_addr, details_json
            )
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
            params![
                local_timestamp(),
                user_id,
                device_id,
                event,
                remote_addr,
                details_json
            ],
        )
        .context("failed to insert mobile device enrollment audit event")?;
    Ok(())
}

fn ensure_cloudflare_mobile_device_ca(
    config: &CloudflareAccessConfig,
    ca_cert_path: &Path,
) -> Result<()> {
    let Some(request) = DeviceCaTrustRequest::from_config(config, ca_cert_path)? else {
        return Ok(());
    };
    request.execute()
}

pub fn sync_device_common_name(
    config: &CloudflareAccessConfig,
    common_name: &str,
    action: DevicePolicyAction,
) -> Result<()> {
    let Some(request) = DevicePolicyRequest::from_config(config, common_name, action) else {
        return Ok(());
    };
    request.execute()
}

#[derive(Debug)]
struct DevicePolicyRequest {
    account_id: String,
    app_id: Option<String>,
    policy_id: String,
    api_token: String,
    common_name: String,
    action: DevicePolicyAction,
}

#[derive(Debug)]
struct DeviceCaTrustRequest {
    account_id: String,
    zone_id: Option<String>,
    api_token: String,
    hostname: String,
    configured_certificate_id: Option<String>,
    ca_cert_path: PathBuf,
    ca_cert_pem: String,
    ca_name: String,
}

impl DeviceCaTrustRequest {
    fn from_config(config: &CloudflareAccessConfig, ca_cert_path: &Path) -> Result<Option<Self>> {
        let Some(account_id) = trimmed(&config.account_id) else {
            return Ok(None);
        };
        let Some(api_token) = trimmed(&config.api_token) else {
            return Ok(None);
        };
        let Some(hostname) = trimmed(&config.mobile_app.hostname) else {
            return Ok(None);
        };
        if !config.mobile_app.enabled {
            return Ok(None);
        }
        let ca_cert_pem = fs::read_to_string(ca_cert_path).with_context(|| {
            format!(
                "failed to read device CA certificate {}",
                ca_cert_path.display()
            )
        })?;
        let fingerprint = certificate_pem_fingerprint(&ca_cert_pem);
        Ok(Some(Self {
            account_id,
            zone_id: trimmed(&config.zone_id),
            api_token,
            hostname,
            configured_certificate_id: trimmed(&config.mobile_device_ca_certificate_id),
            ca_cert_path: ca_cert_path.to_path_buf(),
            ca_cert_pem,
            ca_name: format!("session-manager-mobile-device-ca-{}", &fingerprint[..16]),
        }))
    }

    fn execute(&self) -> Result<()> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let certificate_id = match &self.configured_certificate_id {
            Some(certificate_id) => certificate_id.clone(),
            None => self.ensure_uploaded_ca(&agent)?,
        };
        let zone_id = match &self.zone_id {
            Some(zone_id) => zone_id.clone(),
            None => self.lookup_zone_id(&agent)?,
        };
        self.ensure_hostname_association(&agent, &zone_id, &certificate_id)
    }

    fn ensure_uploaded_ca(&self, agent: &ureq::Agent) -> Result<String> {
        if let Some(certificate_id) = self.find_uploaded_ca(agent)? {
            return Ok(certificate_id);
        }
        let url = format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/mtls_certificates",
            self.account_id
        );
        let body = json!({
            "name": self.ca_name,
            "ca": true,
            "certificates": self.ca_cert_pem,
        });
        let response = cloudflare_post(agent, &url, &self.api_token, body)?;
        cloudflare_result_id(&response).with_context(|| {
            format!(
                "Cloudflare mTLS upload response missing certificate id for {}",
                self.ca_cert_path.display()
            )
        })
    }

    fn find_uploaded_ca(&self, agent: &ureq::Agent) -> Result<Option<String>> {
        let url = format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/mtls_certificates?type=custom",
            self.account_id
        );
        let response = cloudflare_get(agent, &url, &self.api_token)?;
        let Some(results) = response.get("result").and_then(Value::as_array) else {
            bail!("Cloudflare mTLS certificate list response missing result array");
        };
        for certificate in results {
            let name = certificate.get("name").and_then(Value::as_str);
            if name == Some(self.ca_name.as_str()) {
                if let Some(id) = certificate.get("id").and_then(Value::as_str) {
                    return Ok(Some(id.to_owned()));
                }
            }
        }
        Ok(None)
    }

    fn lookup_zone_id(&self, agent: &ureq::Agent) -> Result<String> {
        let zone_name = parent_zone_name(&self.hostname)?;
        let url = format!(
            "https://api.cloudflare.com/client/v4/zones?name={zone_name}&account.id={}",
            self.account_id
        );
        let response = cloudflare_get(agent, &url, &self.api_token)?;
        let zones = response
            .get("result")
            .and_then(Value::as_array)
            .context("Cloudflare zone lookup response missing result array")?;
        let Some(zone) = zones.first() else {
            bail!(
                "Cloudflare zone lookup found no zone for mobile_app.hostname {}; set cloudflare_access.zone_id explicitly",
                self.hostname
            );
        };
        zone.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("Cloudflare zone lookup response missing zone id")
    }

    fn ensure_hostname_association(
        &self,
        agent: &ureq::Agent,
        zone_id: &str,
        certificate_id: &str,
    ) -> Result<()> {
        let url = format!(
            "https://api.cloudflare.com/client/v4/zones/{zone_id}/certificate_authorities/hostname_associations?mtls_certificate_id={certificate_id}"
        );
        let response = cloudflare_get(agent, &url, &self.api_token)?;
        let mut hostnames = hostname_associations_from_response(&response)?;
        if hostnames.iter().any(|hostname| hostname == &self.hostname) {
            return Ok(());
        }
        hostnames.push(self.hostname.clone());
        hostnames.sort();
        hostnames.dedup();
        let update_url = format!(
            "https://api.cloudflare.com/client/v4/zones/{zone_id}/certificate_authorities/hostname_associations"
        );
        cloudflare_put(
            agent,
            &update_url,
            &self.api_token,
            json!({
                "hostnames": hostnames,
                "mtls_certificate_id": certificate_id,
            }),
        )?;
        Ok(())
    }
}

impl DevicePolicyRequest {
    fn from_config(
        config: &CloudflareAccessConfig,
        common_name: &str,
        action: DevicePolicyAction,
    ) -> Option<Self> {
        let account_id = trimmed(&config.account_id)?;
        let policy_id = trimmed(&config.mobile_device_policy_id)?;
        let api_token = trimmed(&config.api_token)?;
        Some(Self {
            account_id,
            app_id: trimmed(&config.mobile_app.app_id),
            policy_id,
            api_token,
            common_name: common_name.trim().to_owned(),
            action,
        })
    }

    fn execute(&self) -> Result<()> {
        if self.common_name.is_empty() {
            bail!("Cloudflare mobile device policy sync requires a non-empty common name");
        }
        let url = self.policy_url();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let current = cloudflare_get(&agent, &url, &self.api_token)?;
        let mut policy = current
            .get("result")
            .cloned()
            .context("Cloudflare Access policy response missing result")?;
        mutate_policy_common_name_allowlist(&mut policy, &self.common_name, self.action)?;
        let payload = policy_update_payload(policy)?;
        cloudflare_put(&agent, &url, &self.api_token, payload)?;
        Ok(())
    }

    fn policy_url(&self) -> String {
        if let Some(app_id) = &self.app_id {
            format!(
                "https://api.cloudflare.com/client/v4/accounts/{}/access/apps/{}/policies/{}",
                self.account_id, app_id, self.policy_id
            )
        } else {
            format!(
                "https://api.cloudflare.com/client/v4/accounts/{}/access/policies/{}",
                self.account_id, self.policy_id
            )
        }
    }
}

fn cloudflare_get(agent: &ureq::Agent, url: &str, api_token: &str) -> Result<Value> {
    let response = agent
        .get(url)
        .header("Authorization", format!("Bearer {api_token}"))
        .header("Content-Type", "application/json")
        .call()
        .with_context(|| format!("Cloudflare API request failed: {url}"))?;
    parse_cloudflare_response(response)
}

fn cloudflare_post(agent: &ureq::Agent, url: &str, api_token: &str, body: Value) -> Result<Value> {
    let response = agent
        .post(url)
        .header("Authorization", format!("Bearer {api_token}"))
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
        .with_context(|| format!("Cloudflare API request failed: {url}"))?;
    parse_cloudflare_response(response)
}

fn cloudflare_put(agent: &ureq::Agent, url: &str, api_token: &str, body: Value) -> Result<Value> {
    let response = agent
        .put(url)
        .header("Authorization", format!("Bearer {api_token}"))
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
        .with_context(|| format!("Cloudflare API request failed: {url}"))?;
    parse_cloudflare_response(response)
}

fn parse_cloudflare_response(mut response: ureq::http::Response<ureq::Body>) -> Result<Value> {
    let status = response.status().as_u16();
    let response_body = response
        .body_mut()
        .read_to_string()
        .context("Cloudflare API response body was unreadable")?;
    let value = serde_json::from_str::<Value>(&response_body)
        .context("Cloudflare API returned non-JSON response")?;
    if status >= 400 || value.get("success").and_then(Value::as_bool) != Some(true) {
        bail!("Cloudflare API request failed with status {status}: {value}");
    }
    Ok(value)
}

fn mutate_policy_common_name_allowlist(
    policy: &mut Value,
    common_name: &str,
    action: DevicePolicyAction,
) -> Result<()> {
    let include = policy
        .get_mut("include")
        .and_then(Value::as_array_mut)
        .context("Cloudflare Access policy result missing include array")?;
    include.retain(|rule| rule.get("certificate").is_none());
    include.retain(|rule| {
        common_name_from_rule(rule)
            .map(|value| value != common_name && value != EMPTY_DEVICE_COMMON_NAME)
            .unwrap_or(true)
    });
    if action == DevicePolicyAction::Allow {
        include.push(json!({"common_name": {"common_name": common_name}}));
    }
    if !include
        .iter()
        .any(|rule| common_name_from_rule(rule).is_some())
    {
        include.push(json!({"common_name": {"common_name": EMPTY_DEVICE_COMMON_NAME}}));
    }
    Ok(())
}

fn common_name_from_rule(rule: &Value) -> Option<&str> {
    rule.get("common_name")
        .and_then(|value| value.get("common_name"))
        .and_then(Value::as_str)
}

fn policy_update_payload(policy: Value) -> Result<Value> {
    let object = policy
        .as_object()
        .context("Cloudflare Access policy result must be an object")?;
    let mut payload = Map::new();
    for key in [
        "name",
        "decision",
        "include",
        "exclude",
        "require",
        "precedence",
        "session_duration",
        "approval_groups",
        "approval_required",
        "purpose_justification_required",
        "purpose_justification_prompt",
        "isolation_required",
    ] {
        if let Some(value) = object.get(key) {
            payload.insert(key.to_owned(), value.clone());
        }
    }
    if !payload.contains_key("name")
        || !payload.contains_key("decision")
        || !payload.contains_key("include")
    {
        bail!("Cloudflare Access policy result missing required update fields");
    }
    Ok(Value::Object(payload))
}

fn certificate_pem_fingerprint(certificate_pem: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(certificate_pem.trim().as_bytes());
    hex_bytes(&hasher.finalize())
}

fn cloudflare_result_id(response: &Value) -> Option<String> {
    response
        .get("result")
        .and_then(|result| result.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn hostname_associations_from_response(response: &Value) -> Result<Vec<String>> {
    let result = response
        .get("result")
        .context("Cloudflare hostname association response missing result")?;
    let mut hostnames = Vec::new();
    collect_hostnames(result, &mut hostnames);
    hostnames.sort();
    hostnames.dedup();
    Ok(hostnames)
}

fn collect_hostnames(value: &Value, hostnames: &mut Vec<String>) {
    match value {
        Value::String(hostname) => {
            if !hostname.trim().is_empty() {
                hostnames.push(hostname.trim().to_owned());
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_hostnames(value, hostnames);
            }
        }
        Value::Object(object) => {
            if let Some(hostname) = object.get("hostname").and_then(Value::as_str) {
                if !hostname.trim().is_empty() {
                    hostnames.push(hostname.trim().to_owned());
                }
            }
            if let Some(values) = object.get("hostnames") {
                collect_hostnames(values, hostnames);
            }
        }
        _ => {}
    }
}

fn parent_zone_name(hostname: &str) -> Result<String> {
    let labels: Vec<&str> = hostname
        .trim()
        .trim_end_matches('.')
        .split('.')
        .filter(|label| !label.is_empty())
        .collect();
    if labels.len() < 2 {
        bail!("Cloudflare mobile_app.hostname must be a fully qualified hostname");
    }
    Ok(format!(
        "{}.{}",
        labels[labels.len() - 2],
        labels[labels.len() - 1]
    ))
}

fn extract_public_key_from_csr_with_openssl(csr_pem: &str) -> Result<String> {
    let temp_dir = temporary_dir("sm-mobile-csr")?;
    let csr_path = temp_dir.join("device.csr.pem");
    fs::write(&csr_path, csr_pem).context("failed to write device CSR")?;
    verify_csr_self_signature_with_openssl(&csr_path)?;
    let output = Command::new("openssl")
        .arg("req")
        .arg("-in")
        .arg(&csr_path)
        .arg("-pubkey")
        .arg("-noout")
        .output()
        .context("failed to invoke openssl for CSR public key extraction")?;
    let _ = fs::remove_dir_all(&temp_dir);
    if !output.status.success() {
        bail!("openssl failed to read CSR public key");
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .context("openssl returned non-UTF-8 CSR public key")
}

fn validate_cloudflare_client_csr_public_key(public_key_pem: &str) -> Result<()> {
    let temp_dir = temporary_dir("sm-mobile-csr-public-key")?;
    let public_key_path = temp_dir.join("device.pub.pem");
    fs::write(&public_key_path, public_key_pem).context("failed to write device CSR public key")?;
    let output = Command::new("openssl")
        .arg("pkey")
        .arg("-pubin")
        .arg("-in")
        .arg(&public_key_path)
        .arg("-noout")
        .arg("-text")
        .output()
        .context("failed to invoke openssl for CSR public key type check")?;
    let _ = fs::remove_dir_all(&temp_dir);
    if !output.status.success() {
        bail!("openssl failed to inspect CSR public key");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let has_rsa_modulus = text.lines().any(|line| line.trim_start() == "Modulus:");
    let has_rsa_exponent = text
        .lines()
        .any(|line| line.trim_start().starts_with("Exponent:"));
    if has_rsa_modulus && has_rsa_exponent {
        Ok(())
    } else {
        bail!("Cloudflare client certificate CSR public key must be RSA")
    }
}

fn verify_csr_self_signature_with_openssl(csr_path: &Path) -> Result<()> {
    let output = Command::new("openssl")
        .arg("req")
        .arg("-in")
        .arg(csr_path)
        .arg("-verify")
        .arg("-noout")
        .output()
        .context("failed to invoke openssl for CSR verification")?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "openssl failed to verify CSR self-signature: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn sign_csr_with_openssl(
    ca_cert_path: &Path,
    ca_key_path: &Path,
    csr_pem: &str,
    common_name: &str,
) -> Result<String> {
    let temp_dir = temporary_dir("sm-mobile-cert")?;
    let csr_path = temp_dir.join("device.csr.pem");
    let cert_path = temp_dir.join("device.cert.pem");
    let ext_path = temp_dir.join("client.ext");
    let serial_path = temp_dir.join("device.srl");
    fs::write(&csr_path, csr_pem).context("failed to write device CSR")?;
    fs::write(
        &ext_path,
        r#"
[client_cert]
basicConstraints = critical,CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid,issuer
"#,
    )
    .context("failed to write client certificate extension file")?;
    let status = Command::new("openssl")
        .arg("x509")
        .arg("-req")
        .arg("-in")
        .arg(&csr_path)
        .arg("-CA")
        .arg(ca_cert_path)
        .arg("-CAkey")
        .arg(ca_key_path)
        .arg("-CAcreateserial")
        .arg("-CAserial")
        .arg(&serial_path)
        .arg("-out")
        .arg(&cert_path)
        .arg("-subj")
        .arg(format!("/CN={common_name}"))
        .arg("-days")
        .arg("3650")
        .arg("-sha256")
        .arg("-extfile")
        .arg(&ext_path)
        .arg("-extensions")
        .arg("client_cert")
        .status()
        .context("failed to invoke openssl for CSR signing")?;
    if !status.success() {
        let _ = fs::remove_dir_all(&temp_dir);
        bail!("openssl failed to sign CSR");
    }
    let certificate_pem =
        fs::read_to_string(&cert_path).context("failed to read signed certificate");
    let _ = fs::remove_dir_all(&temp_dir);
    certificate_pem
}

fn ensure_device_ca(ca_cert_path: &Path, ca_key_path: &Path) -> Result<()> {
    let cert_exists = ca_cert_path.exists();
    let key_exists = ca_key_path.exists();
    match (cert_exists, key_exists) {
        (true, true) => return Ok(()),
        (true, false) | (false, true) => {
            bail!(
                "mobile device CA files must both exist or both be absent: {} and {}",
                ca_cert_path.display(),
                ca_key_path.display()
            );
        }
        (false, false) => {}
    }

    if let Some(parent) = ca_cert_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create CA certificate dir {}", parent.display()))?;
    }
    if let Some(parent) = ca_key_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create CA private key dir {}", parent.display()))?;
    }
    let output = Command::new("openssl")
        .arg("req")
        .arg("-x509")
        .arg("-newkey")
        .arg("rsa:2048")
        .arg("-nodes")
        .arg("-sha256")
        .arg("-days")
        .arg("3650")
        .arg("-subj")
        .arg("/CN=Session Manager Mobile Device CA")
        .arg("-addext")
        .arg("basicConstraints=critical,CA:TRUE,pathlen:0")
        .arg("-addext")
        .arg("keyUsage=critical,keyCertSign,cRLSign")
        .arg("-keyout")
        .arg(ca_key_path)
        .arg("-out")
        .arg(ca_cert_path)
        .output()
        .context("failed to invoke openssl for mobile device CA generation")?;
    if !output.status.success() {
        let _ = fs::remove_file(ca_cert_path);
        let _ = fs::remove_file(ca_key_path);
        bail!(
            "openssl failed to generate mobile device CA: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    restrict_private_key_permissions(ca_key_path)?;
    Ok(())
}

fn restrict_private_key_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!(
                "failed to restrict private key permissions {}",
                path.display()
            )
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn build_certificate_chain_pem(certificate_pem: &str, ca_cert_path: &Path) -> Result<String> {
    let ca_cert_pem =
        fs::read_to_string(ca_cert_path).context("failed to read device CA certificate")?;
    Ok(format!(
        "{}\n{}",
        certificate_pem.trim(),
        ca_cert_pem.trim()
    ))
}

fn temporary_dir(prefix: &str) -> Result<PathBuf> {
    let mut bytes = [0u8; 12];
    OsRng.fill_bytes(&mut bytes);
    let path = std::env::temp_dir().join(format!("{prefix}-{}", hex_bytes(&bytes)));
    fs::create_dir(&path)
        .with_context(|| format!("failed to create temporary directory {}", path.display()))?;
    Ok(path)
}

fn pairing_base_url(listen: SocketAddr) -> String {
    let port = listen.port();
    let host = if listen.ip().is_unspecified() {
        local_lan_ip().unwrap_or_else(|| "127.0.0.1".to_owned())
    } else {
        listen.ip().to_string()
    };
    format!("http://{host}:{port}")
}

fn request_base_url(state: &PairingState, headers: &HeaderMap) -> String {
    let advertised = state.advertised_base_url.trim();
    if !advertised.is_empty() {
        return advertised.to_owned();
    }
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("127.0.0.1:19192");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| matches!(*value, "http" | "https"))
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

fn local_lan_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("1.1.1.1:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

fn qr_ascii(value: &str) -> Result<String> {
    let code = QrCode::new(value.as_bytes()).context("failed to render enrollment QR")?;
    Ok(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
}

fn enrollment_redirect_html(enrollment_url: &str) -> String {
    let encoded_url = percent_encode_query_value(enrollment_url);
    let app_link = format!("sm-enroll://enroll?url={encoded_url}");
    let intent_link = format!(
        "intent://enroll?url={encoded_url}#Intent;scheme=sm-enroll;package=li.rajeshgo.sm;S.browser_fallback_url={encoded_url};end"
    );
    let intent_js = serde_json::to_string(&intent_link).unwrap_or_else(|_| "\"\"".to_owned());
    format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Open Session Manager</title>
  <script>
    window.location.href = {intent_js};
  </script>
</head>
<body>
  <h1>Open Session Manager</h1>
  <p>This enrollment link must be opened in the Session Manager Android app.</p>
  <p><a href="{intent_link}">Open Session Manager</a></p>
  <p><a href="{app_link}">Open with app link</a></p>
  <p>If the button does not work, scan the QR again with the phone Camera app and open it in Session Manager.</p>
  <p>Enrollment URL: <code>{enrollment_url}</code></p>
</body>
</html>"#,
        intent_link = escape_html_attr(&intent_link),
        app_link = escape_html_attr(&app_link),
        enrollment_url = escape_html(enrollment_url),
    )
}

fn percent_encode_query_value(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_html_attr(value: &str) -> String {
    escape_html(value).replace('"', "&quot;")
}

fn valid_device_id(value: &str) -> bool {
    let len = value.len();
    (3..=128).contains(&len)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn public_key_fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.trim().as_bytes());
    hex_bytes(&digest)
}

fn validate_mobile_terminal_proof_public_key(public_key_pem: &str) -> Result<()> {
    VerifyingKey::from_public_key_pem(public_key_pem.trim())
        .map(|_| ())
        .context("mobile terminal proof public key must be a P-256 public key")
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn random_urlsafe_token(byte_len: usize) -> String {
    let mut bytes = vec![0u8; byte_len];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn local_timestamp() -> String {
    time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::{
        ecdsa::SigningKey,
        pkcs8::{EncodePublicKey, LineEnding},
    };

    #[test]
    fn enrollment_db_round_trip_and_revoke() {
        let temp_dir = temporary_dir("sm-mobile-test").expect("temp dir");
        let db_path = temp_dir.join("mobile_devices.db");
        let pairing = create_pairing_registration(&db_path, "rajesh", 15).expect("create pairing");
        assert_eq!(pairing.user_id, "rajesh");
        let completed = complete_pairing_registration(
            &db_path,
            &pairing.token,
            "android-abc",
            "Pixel",
            "-----BEGIN PUBLIC KEY-----\nabc\n-----END PUBLIC KEY-----",
            Some("127.0.0.1:1"),
        )
        .expect("complete")
        .expect("completed");
        assert_eq!(completed.token, pairing.token);
        assert!(active_device_exists(&db_path, "rajesh", "android-abc").expect("exists"));
        assert!(active_device_public_key(&db_path, "rajesh", "android-abc")
            .expect("public key")
            .is_some());
        assert!(revoke_device(&db_path, "rajesh", "android-abc").expect("revoke"));
        assert!(!active_device_exists(&db_path, "rajesh", "android-abc").expect("revoked"));
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn enrollment_proof_public_key_requires_p256_key() {
        let signing_key = SigningKey::random(&mut OsRng);
        let public_key = signing_key
            .verifying_key()
            .to_public_key_pem(LineEnding::LF)
            .expect("public key pem");

        validate_mobile_terminal_proof_public_key(&public_key).expect("p256 key accepted");
        validate_mobile_terminal_proof_public_key(
            "-----BEGIN PUBLIC KEY-----\ninvalid\n-----END PUBLIC KEY-----",
        )
        .expect_err("invalid key rejected");
    }

    #[test]
    fn enrollment_csr_public_key_requires_rsa_for_cloudflare_mtls() {
        let rsa_public_key = openssl_public_key_pem("rsa").expect("rsa public key");
        let ec_public_key = openssl_public_key_pem("ec").expect("ec public key");

        validate_cloudflare_client_csr_public_key(&rsa_public_key).expect("rsa key accepted");
        validate_cloudflare_client_csr_public_key(&ec_public_key).expect_err("ec key rejected");
    }

    #[test]
    fn ensure_device_ca_generates_missing_pair() {
        let temp_dir = temporary_dir("sm-mobile-ca-test").expect("temp dir");
        let cert_path = temp_dir.join("device-ca.pem");
        let key_path = temp_dir.join("device-ca.key");

        ensure_device_ca(&cert_path, &key_path).expect("generate CA");

        let cert = fs::read_to_string(&cert_path).expect("read cert");
        let key = fs::read_to_string(&key_path).expect("read key");
        assert!(cert.contains("BEGIN CERTIFICATE"));
        assert!(key.contains("BEGIN PRIVATE KEY"));
        #[cfg(unix)]
        {
            let mode = fs::metadata(&key_path)
                .expect("key metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(temp_dir);
    }

    fn openssl_public_key_pem(kind: &str) -> Result<String> {
        let temp_dir = temporary_dir("sm-mobile-key-test")?;
        let key_path = temp_dir.join(format!("{kind}.key"));
        let public_key_path = temp_dir.join(format!("{kind}.pub.pem"));
        let key_status = if kind == "rsa" {
            Command::new("openssl")
                .arg("genrsa")
                .arg("-out")
                .arg(&key_path)
                .arg("2048")
                .status()
                .context("failed to invoke openssl genrsa")?
        } else {
            Command::new("openssl")
                .arg("ecparam")
                .arg("-name")
                .arg("prime256v1")
                .arg("-genkey")
                .arg("-noout")
                .arg("-out")
                .arg(&key_path)
                .status()
                .context("failed to invoke openssl ecparam")?
        };
        if !key_status.success() {
            bail!("openssl failed to generate {kind} key");
        }
        let output_status = Command::new("openssl")
            .arg("pkey")
            .arg("-in")
            .arg(&key_path)
            .arg("-pubout")
            .arg("-out")
            .arg(&public_key_path)
            .status()
            .context("failed to invoke openssl public key export")?;
        if !output_status.success() {
            bail!("openssl failed to export {kind} public key");
        }
        let public_key =
            fs::read_to_string(&public_key_path).context("failed to read generated public key")?;
        let _ = fs::remove_dir_all(temp_dir);
        Ok(public_key)
    }

    #[test]
    fn cloudflare_policy_sync_removes_broad_certificate_rule() {
        let mut policy = json!({
            "include": [
                {"certificate": {}},
                {"common_name": {"common_name": "old-device"}}
            ]
        });

        mutate_policy_common_name_allowlist(&mut policy, "android-new", DevicePolicyAction::Allow)
            .expect("mutate policy");

        assert_eq!(
            policy["include"],
            json!([
                {"common_name": {"common_name": "old-device"}},
                {"common_name": {"common_name": "android-new"}}
            ])
        );
    }

    #[test]
    fn cloudflare_policy_revoke_leaves_impossible_rule_when_empty() {
        let mut policy = json!({
            "include": [
                {"common_name": {"common_name": "android-old"}}
            ]
        });

        mutate_policy_common_name_allowlist(&mut policy, "android-old", DevicePolicyAction::Revoke)
            .expect("mutate policy");

        assert_eq!(
            policy["include"],
            json!([
                {"common_name": {"common_name": EMPTY_DEVICE_COMMON_NAME}}
            ])
        );
    }

    #[test]
    fn cloudflare_ca_trust_request_derives_name_and_zone_from_mobile_host() {
        let temp_dir = temporary_dir("sm-test-ca").expect("temp dir");
        let ca_path = temp_dir.join("ca.pem");
        fs::write(
            &ca_path,
            "-----BEGIN CERTIFICATE-----\nfixture\n-----END CERTIFICATE-----\n",
        )
        .expect("write ca");
        let mut config = CloudflareAccessConfig {
            account_id: Some("account".to_owned()),
            api_token: Some("token".to_owned()),
            mobile_device_policy_id: Some("policy".to_owned()),
            ..CloudflareAccessConfig::default()
        };
        config.mobile_app.enabled = true;
        config.mobile_app.hostname = Some("sm-app.rajeshgo.li".to_owned());

        let request = DeviceCaTrustRequest::from_config(&config, &ca_path)
            .expect("request result")
            .expect("request configured");

        assert_eq!(request.account_id, "account");
        assert_eq!(request.hostname, "sm-app.rajeshgo.li");
        assert_eq!(parent_zone_name(&request.hostname).unwrap(), "rajeshgo.li");
        assert!(request
            .ca_name
            .starts_with("session-manager-mobile-device-ca-"));
        assert!(request.configured_certificate_id.is_none());
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn cloudflare_hostname_association_parser_accepts_common_response_shapes() {
        let from_strings = hostname_associations_from_response(&json!({
            "result": ["sm-app.rajeshgo.li", "sm-node.rajeshgo.li"]
        }))
        .expect("parse strings");
        assert_eq!(
            from_strings,
            vec!["sm-app.rajeshgo.li", "sm-node.rajeshgo.li"]
        );

        let from_objects = hostname_associations_from_response(&json!({
            "result": {
                "hostnames": [
                    {"hostname": "sm-app.rajeshgo.li"},
                    {"hostname": "sm-app.rajeshgo.li"},
                    "sm-node.rajeshgo.li"
                ]
            }
        }))
        .expect("parse objects");
        assert_eq!(
            from_objects,
            vec!["sm-app.rajeshgo.li", "sm-node.rajeshgo.li"]
        );
    }

    #[test]
    fn enrollment_path_matches_android_qr_contract() {
        assert_eq!(
            enrollment_path("abc123"),
            "/client/mobile-terminal/enroll/abc123"
        );
    }

    #[test]
    fn qr_ascii_uses_dense_unicode_blocks() {
        let qr = qr_ascii("http://192.168.4.31:19192/client/mobile-terminal/enroll/abc123")
            .expect("render QR");

        assert!(!qr.contains('#'));
        assert!(qr.contains('█') || qr.contains('▀') || qr.contains('▄'));
    }

    #[test]
    fn request_base_url_prefers_advertised_url() {
        let state = PairingState {
            config: AppConfig::default(),
            db_path: PathBuf::new(),
            ca_cert_path: PathBuf::new(),
            ca_key_path: PathBuf::new(),
            advertised_base_url: "https://sm-app.rajeshgo.li/pair".to_owned(),
            unknown_attempts: Arc::new(Mutex::new(0)),
            shutdown: Arc::new(Notify::new()),
        };
        let mut headers = HeaderMap::new();
        headers.insert(HOST, "127.0.0.1:19192".parse().expect("host header"));

        assert_eq!(
            request_base_url(&state, &headers),
            "https://sm-app.rajeshgo.li/pair"
        );
    }

    #[test]
    fn request_base_url_falls_back_to_forwarded_proto_and_host() {
        let state = PairingState {
            config: AppConfig::default(),
            db_path: PathBuf::new(),
            ca_cert_path: PathBuf::new(),
            ca_key_path: PathBuf::new(),
            advertised_base_url: String::new(),
            unknown_attempts: Arc::new(Mutex::new(0)),
            shutdown: Arc::new(Notify::new()),
        };
        let mut headers = HeaderMap::new();
        headers.insert(HOST, "pairing.internal:19192".parse().expect("host header"));
        headers.insert("x-forwarded-proto", "https".parse().expect("proto header"));

        assert_eq!(
            request_base_url(&state, &headers),
            "https://pairing.internal:19192"
        );
    }

    #[test]
    fn enrollment_redirect_html_contains_android_intent_deep_link() {
        let url = "http://192.168.4.31:19192/client/mobile-terminal/enroll/abc123";
        let html = enrollment_redirect_html(url);

        assert!(html.contains("intent://enroll?url=http%3A%2F%2F192.168.4.31%3A19192%2Fclient%2Fmobile-terminal%2Fenroll%2Fabc123#Intent;scheme=sm-enroll;package=li.rajeshgo.sm;"));
        assert!(html.contains("sm-enroll://enroll?url=http%3A%2F%2F192.168.4.31%3A19192%2Fclient%2Fmobile-terminal%2Fenroll%2Fabc123"));
        assert!(html.contains("Enrollment URL:"));
    }

    #[test]
    fn percent_encode_query_value_matches_deep_link_needs() {
        assert_eq!(
            percent_encode_query_value("http://host/path?x=1&y=a b"),
            "http%3A%2F%2Fhost%2Fpath%3Fx%3D1%26y%3Da%20b"
        );
    }
}
