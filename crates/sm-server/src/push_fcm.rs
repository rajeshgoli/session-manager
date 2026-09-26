//! Firebase Cloud Messaging sender for owner follow notifications (sm#1569).
//!
//! A Google service account key signs a short-lived JWT, which Google's
//! token endpoint exchanges for an access token; the token authorises FCM
//! HTTP v1 sends. Messages are data-only so the Android app always builds
//! the notification itself.

use std::{collections::BTreeMap, fs, path::Path, sync::Mutex, time::Duration as StdDuration};

use anyhow::{anyhow, Context, Result};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};

use crate::owner_push::{PushError, PushSender};

const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_FCM_BASE_URL: &str = "https://fcm.googleapis.com";
const REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(10);
/// Refresh the access token this long before Google says it expires.
const TOKEN_REFRESH_MARGIN: Duration = Duration::minutes(5);
/// How long Google holds a push for a phone that is offline.
const MESSAGE_TTL: &str = "86400s";

#[derive(Debug, Deserialize)]
struct ServiceAccountKey {
    project_id: String,
    client_email: String,
    private_key: String,
    #[serde(default)]
    token_uri: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JwtClaims {
    pub iss: String,
    pub scope: String,
    pub aud: String,
    pub iat: i64,
    pub exp: i64,
}

pub struct FcmSender {
    project_id: String,
    client_email: String,
    encoding_key: EncodingKey,
    token_uri: String,
    fcm_base_url: String,
    access_token: Mutex<Option<(String, OffsetDateTime)>>,
}

impl std::fmt::Debug for FcmSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FcmSender")
            .field("project_id", &self.project_id)
            .field("client_email", &self.client_email)
            .finish_non_exhaustive()
    }
}

impl FcmSender {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let key: ServiceAccountKey = serde_json::from_str(&raw)
            .with_context(|| format!("{} is not a service account key", path.display()))?;
        if key.project_id.trim().is_empty() || key.client_email.trim().is_empty() {
            return Err(anyhow!(
                "{} lacks project_id or client_email",
                path.display()
            ));
        }
        let encoding_key = EncodingKey::from_rsa_pem(key.private_key.as_bytes())
            .with_context(|| format!("{} has an unusable private_key", path.display()))?;
        Ok(Self {
            project_id: key.project_id.trim().to_owned(),
            client_email: key.client_email.trim().to_owned(),
            encoding_key,
            token_uri: key
                .token_uri
                .filter(|uri| !uri.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_owned()),
            fcm_base_url: DEFAULT_FCM_BASE_URL.to_owned(),
            access_token: Mutex::new(None),
        })
    }

    /// Loads the sender named by `push.fcm.service_account_path`; `None`
    /// when push is not configured.
    pub fn from_config(config: &crate::config::AppConfig) -> Option<Result<Self>> {
        let path = config
            .push
            .fcm
            .service_account_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())?;
        Some(Self::load(&crate::sessions::expand_home(path)))
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[cfg(test)]
    fn with_fcm_base_url(mut self, base_url: &str) -> Self {
        self.fcm_base_url = base_url.trim_end_matches('/').to_owned();
        self
    }

    pub fn jwt_claims(&self, now: OffsetDateTime) -> JwtClaims {
        let iat = now.unix_timestamp();
        JwtClaims {
            iss: self.client_email.clone(),
            scope: FCM_SCOPE.to_owned(),
            aud: self.token_uri.clone(),
            iat,
            exp: iat + 3600,
        }
    }

    fn signed_assertion(&self, now: OffsetDateTime) -> Result<String> {
        jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &self.jwt_claims(now),
            &self.encoding_key,
        )
        .context("failed to sign FCM token request")
    }

    fn access_token(&self, now: OffsetDateTime) -> Result<String, PushError> {
        let mut cached = self
            .access_token
            .lock()
            .map_err(|_| PushError::Retryable("FCM token cache poisoned".to_owned()))?;
        if let Some((token, expires_at)) = cached.as_ref() {
            if now + TOKEN_REFRESH_MARGIN < *expires_at {
                return Ok(token.clone());
            }
        }
        let assertion = self
            .signed_assertion(now)
            .map_err(|error| PushError::Retryable(format!("{error:#}")))?;
        let (status, body) = http_post(
            &self.token_uri,
            None,
            "application/x-www-form-urlencoded",
            format!(
                "grant_type={}&assertion={assertion}",
                "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"
            )
            .into_bytes(),
        )?;
        if status >= 400 {
            return Err(PushError::Retryable(format!(
                "FCM token exchange failed ({status}): {body}"
            )));
        }
        let payload: Value = serde_json::from_str(&body).map_err(|error| {
            PushError::Retryable(format!("FCM token response unreadable: {error}"))
        })?;
        let token = payload
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| PushError::Retryable("FCM token response lacks access_token".into()))?
            .to_owned();
        let expires_in = payload
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(3600);
        *cached = Some((token.clone(), now + Duration::seconds(expires_in)));
        Ok(token)
    }

    /// The FCM HTTP v1 request body for one device.
    pub fn message_body(token: &str, data: &BTreeMap<String, String>) -> Value {
        json!({
            "message": {
                "token": token,
                "android": {"priority": "HIGH", "ttl": MESSAGE_TTL},
                "data": data,
            }
        })
    }
}

impl PushSender for FcmSender {
    fn send(&self, token: &str, data: &BTreeMap<String, String>) -> Result<(), PushError> {
        let access_token = self.access_token(OffsetDateTime::now_utc())?;
        let endpoint = format!(
            "{}/v1/projects/{}/messages:send",
            self.fcm_base_url, self.project_id
        );
        let body = serde_json::to_vec(&Self::message_body(token, data))
            .map_err(|error| PushError::Retryable(error.to_string()))?;
        let (status, response) =
            http_post(&endpoint, Some(&access_token), "application/json", body)?;
        if status < 300 {
            return Ok(());
        }
        if status == 401 {
            // A revoked or expired access token: fetch a fresh one next time.
            if let Ok(mut cached) = self.access_token.lock() {
                *cached = None;
            }
        }
        Err(classify_send_error(status, &response))
    }
}

/// Maps an FCM error response to what the worker does with the token.
pub fn classify_send_error(status: u16, body: &str) -> PushError {
    let detail = format!("FCM send failed ({status}): {}", body.trim());
    let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = &payload["error"];
    let error_codes = error["details"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|detail| detail["errorCode"].as_str())
        .collect::<Vec<_>>();
    let message = error["message"].as_str().unwrap_or("").to_ascii_lowercase();
    let unregistered = status == 404 || error_codes.contains(&"UNREGISTERED");
    let bad_token = status == 400
        && (error["status"].as_str() == Some("INVALID_ARGUMENT")
            || error_codes.contains(&"INVALID_ARGUMENT"))
        && message.contains("token");
    if unregistered || bad_token {
        PushError::InvalidToken(detail)
    } else {
        PushError::Retryable(detail)
    }
}

fn http_post(
    url: &str,
    bearer: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> Result<(u16, String), PushError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .into();
    let mut request = agent.post(url).header("Content-Type", content_type);
    if let Some(bearer) = bearer {
        request = request.header("Authorization", format!("Bearer {bearer}"));
    }
    let mut response = request
        .send(body.as_slice())
        .map_err(|error| PushError::Retryable(format!("request to {url} failed: {error}")))?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().map_err(|error| {
        PushError::Retryable(format!("response from {url} unreadable: {error}"))
    })?;
    Ok((status, text))
}

#[cfg(test)]
mod tests;
