use axum::http::Uri;
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::config::{CloudflareAccessApplicationConfig, CloudflareAccessConfig};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CloudflareAccessClaims {
    pub sub: String,
    pub exp: usize,
    pub iat: usize,
    pub iss: String,
    #[serde(default)]
    pub common_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudflareAccessError {
    MissingIssuer,
    MissingAudience,
    MalformedToken,
    MissingKeyId,
    InvalidIssuer,
    JwksFetchFailed,
    UnknownKeyId,
    InvalidAssertion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudflareAccessApplication {
    Browser,
    MobileApp,
    NodeFallback,
    EmailWorker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudflareAccessContext {
    pub application: CloudflareAccessApplication,
    pub subject: String,
    pub identity: String,
    pub email: Option<String>,
    pub common_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudflareAccessContextError {
    Disabled,
    IncompleteConfig,
    MissingIdentity,
    InvalidAssertion(CloudflareAccessError),
}

impl From<CloudflareAccessError> for CloudflareAccessContextError {
    fn from(value: CloudflareAccessError) -> Self {
        match value {
            CloudflareAccessError::MissingIssuer | CloudflareAccessError::MissingAudience => {
                CloudflareAccessContextError::IncompleteConfig
            }
            other => CloudflareAccessContextError::InvalidAssertion(other),
        }
    }
}

impl CloudflareAccessApplication {
    pub fn config<'a>(
        self,
        config: &'a CloudflareAccessConfig,
    ) -> &'a CloudflareAccessApplicationConfig {
        match self {
            CloudflareAccessApplication::Browser => &config.browser,
            CloudflareAccessApplication::MobileApp => &config.mobile_app,
            CloudflareAccessApplication::NodeFallback => &config.node_fallback,
            CloudflareAccessApplication::EmailWorker => &config.email_worker,
        }
    }
}

pub fn cloudflare_access_application_for_host(
    config: &CloudflareAccessConfig,
    host: &str,
) -> Option<CloudflareAccessApplication> {
    let hostname = normalize_host_name(host)?;
    [
        CloudflareAccessApplication::Browser,
        CloudflareAccessApplication::MobileApp,
        CloudflareAccessApplication::NodeFallback,
        CloudflareAccessApplication::EmailWorker,
    ]
    .into_iter()
    .find(|application| {
        let app_config = application.config(config);
        app_config.enabled && configured_hostname_matches(app_config, &hostname)
    })
}

pub fn cloudflare_access_has_enabled_app(config: &CloudflareAccessConfig) -> bool {
    [
        CloudflareAccessApplication::Browser,
        CloudflareAccessApplication::MobileApp,
        CloudflareAccessApplication::NodeFallback,
        CloudflareAccessApplication::EmailWorker,
    ]
    .into_iter()
    .any(|application| application.config(config).enabled)
}

pub fn cloudflare_access_has_enabled_app_without_hostname(config: &CloudflareAccessConfig) -> bool {
    [
        CloudflareAccessApplication::Browser,
        CloudflareAccessApplication::MobileApp,
        CloudflareAccessApplication::NodeFallback,
        CloudflareAccessApplication::EmailWorker,
    ]
    .into_iter()
    .any(|application| {
        let app_config = application.config(config);
        app_config.enabled
            && app_config
                .hostname
                .as_deref()
                .and_then(normalize_host_name)
                .is_none()
    })
}

pub fn classify_cloudflare_access_assertion(
    config: &CloudflareAccessConfig,
    application: CloudflareAccessApplication,
    token: &str,
) -> Result<CloudflareAccessContext, CloudflareAccessContextError> {
    let app_config = application.config(config);
    if !app_config.enabled {
        return Err(CloudflareAccessContextError::Disabled);
    }
    let expected_issuer = config
        .expected_issuer()
        .ok_or(CloudflareAccessContextError::IncompleteConfig)?;
    let expected_audience = app_config
        .expected_audience()
        .ok_or(CloudflareAccessContextError::IncompleteConfig)?;
    let claims = verify_cloudflare_access_assertion(token, &expected_issuer, expected_audience)?;
    context_from_claims(config, application, claims)
}

pub fn classify_cloudflare_access_assertion_with_jwks(
    config: &CloudflareAccessConfig,
    application: CloudflareAccessApplication,
    token: &str,
    jwks: &JwkSet,
) -> Result<CloudflareAccessContext, CloudflareAccessContextError> {
    let app_config = application.config(config);
    if !app_config.enabled {
        return Err(CloudflareAccessContextError::Disabled);
    }
    let expected_issuer = config
        .expected_issuer()
        .ok_or(CloudflareAccessContextError::IncompleteConfig)?;
    let expected_audience = app_config
        .expected_audience()
        .ok_or(CloudflareAccessContextError::IncompleteConfig)?;
    let claims = verify_cloudflare_access_assertion_with_jwks(
        token,
        &expected_issuer,
        expected_audience,
        jwks,
    )?;
    context_from_claims(config, application, claims)
}

pub fn fetch_cloudflare_access_jwks(
    expected_issuer: &str,
) -> Result<(String, JwkSet), CloudflareAccessError> {
    let expected_issuer = expected_cloudflare_access_issuer(expected_issuer)?;
    let jwks_url = format!("{expected_issuer}/cdn-cgi/access/certs");
    let mut response = ureq::get(&jwks_url)
        .call()
        .map_err(|_| CloudflareAccessError::JwksFetchFailed)?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|_| CloudflareAccessError::JwksFetchFailed)?;
    let jwks: JwkSet =
        serde_json::from_str(&body).map_err(|_| CloudflareAccessError::JwksFetchFailed)?;
    Ok((expected_issuer, jwks))
}

fn context_from_claims(
    config: &CloudflareAccessConfig,
    application: CloudflareAccessApplication,
    claims: CloudflareAccessClaims,
) -> Result<CloudflareAccessContext, CloudflareAccessContextError> {
    if !application.config(config).enabled {
        return Err(CloudflareAccessContextError::Disabled);
    }
    let subject = claims.sub.trim().to_owned();
    let email = trimmed_string(claims.email);
    let common_name = trimmed_string(claims.common_name);
    let identity = match application {
        CloudflareAccessApplication::Browser => email
            .clone()
            .ok_or(CloudflareAccessContextError::MissingIdentity)?,
        CloudflareAccessApplication::MobileApp | CloudflareAccessApplication::NodeFallback => {
            common_name
                .clone()
                .ok_or(CloudflareAccessContextError::MissingIdentity)?
        }
        CloudflareAccessApplication::EmailWorker => common_name
            .clone()
            .or_else(|| email.clone())
            .or_else(|| (!subject.is_empty()).then(|| subject.clone()))
            .ok_or(CloudflareAccessContextError::MissingIdentity)?,
    };

    Ok(CloudflareAccessContext {
        application,
        subject,
        identity,
        email,
        common_name,
    })
}

pub fn verify_cloudflare_access_assertion_with_jwks(
    token: &str,
    expected_issuer: &str,
    expected_audience: &str,
    jwks: &JwkSet,
) -> Result<CloudflareAccessClaims, CloudflareAccessError> {
    let expected_issuer = expected_cloudflare_access_issuer(expected_issuer)?;
    let expected_audience = expected_audience.trim();
    if expected_audience.is_empty() {
        return Err(CloudflareAccessError::MissingAudience);
    }

    let kid = cloudflare_access_assertion_key_id(token)?;
    let key = DecodingKey::from_jwk(jwks.find(&kid).ok_or(CloudflareAccessError::UnknownKeyId)?)
        .map_err(|_| CloudflareAccessError::InvalidAssertion)?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[expected_audience]);
    validation.set_issuer(&[expected_issuer.as_str()]);
    validation.set_required_spec_claims(&["exp", "iss", "aud"]);
    validation.validate_nbf = true;
    decode::<CloudflareAccessClaims>(token, &key, &validation)
        .map(|decoded| decoded.claims)
        .map_err(|_| CloudflareAccessError::InvalidAssertion)
}

pub fn verify_cloudflare_access_assertion(
    token: &str,
    expected_issuer: &str,
    expected_audience: &str,
) -> Result<CloudflareAccessClaims, CloudflareAccessError> {
    let expected_issuer = expected_cloudflare_access_issuer(expected_issuer)?;
    cloudflare_access_assertion_key_id(token)?;
    let (_, jwks) = fetch_cloudflare_access_jwks(&expected_issuer)?;
    verify_cloudflare_access_assertion_with_jwks(token, &expected_issuer, expected_audience, &jwks)
}

pub fn cloudflare_access_assertion_key_id(token: &str) -> Result<String, CloudflareAccessError> {
    let header = decode_header(token).map_err(|_| CloudflareAccessError::MalformedToken)?;
    header
        .kid
        .filter(|kid| !kid.trim().is_empty())
        .ok_or(CloudflareAccessError::MissingKeyId)
}

fn expected_cloudflare_access_issuer(issuer: &str) -> Result<String, CloudflareAccessError> {
    let issuer = issuer.trim();
    if issuer.is_empty() {
        return Err(CloudflareAccessError::MissingIssuer);
    }
    cloudflare_access_issuer(issuer).ok_or(CloudflareAccessError::InvalidIssuer)
}

fn cloudflare_access_issuer(issuer: &str) -> Option<String> {
    let uri: Uri = issuer.parse().ok()?;
    if uri.scheme_str()? != "https" {
        return None;
    }
    if uri.path() != "/" && !uri.path().is_empty() {
        return None;
    }
    if uri.query().is_some() {
        return None;
    }
    let host = uri.host()?.to_ascii_lowercase();
    if host == "cloudflareaccess.com" || host.ends_with(".cloudflareaccess.com") {
        Some(format!("https://{}", uri.authority()?))
    } else {
        None
    }
}

fn configured_hostname_matches(app_config: &CloudflareAccessApplicationConfig, host: &str) -> bool {
    app_config
        .hostname
        .as_deref()
        .and_then(normalize_host_name)
        .is_some_and(|configured| configured == host)
}

fn normalize_host_name(host: &str) -> Option<String> {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    if host.starts_with('[') {
        return host
            .split(']')
            .next()
            .map(|value| value.trim_start_matches('[').to_owned())
            .filter(|value| !value.is_empty());
    }
    host.split(':')
        .next()
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
}

fn trimmed_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::Serialize;

    const TEST_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDHRDHpPpVmkNOo
SCHqlZMfoO3MNnDcctKIvYDPvCRHXPGD0BcwYn1NMu07kVsigaqsudF2i7ue7qh9
UvnA67rH587OAd96rDBuhtDA5GWuydMlGfGKdVzcv2HnIqS2bdziieGxG8jzMkz0
B95XM/jlaTHWlQq5N/HZ+WtVkUvyRoQxoO1eU2p1qyZ4ryAA/sgwOqx+c5yOaqPb
jPl5GpvA46aCP2s3EFfkKOpNg5VWTJT274I5mp0a1t/yVKckHM3R21rOrErKQ2tt
HED11W4EgiiWUTrc/hXZEPuR/MrVdxKs6njyYt87zl+obXvac4E9rQPQjqBFl0IH
nhU/ZQEfAgMBAAECggEALv9NikaBdCRaV0lT/oDypzYMf+qjKctHDosrc1Nbdx1b
tQwCPB4ukWOegjshNH1CpORam8wPH0gIoy5Ly42NixKIOxxD+incbmULnUMBHH1r
eEerKU3O7h2RWLNaB9DXlPKSMXRtK7bEYZXtgcG3RVxPLd4PHmosd42VHCRdtjEy
fUEP/sr2RpS6L/ItxYxneAtraw3Ro60ZOJNUfTEg0HPj4JA29v16t10AcdwnVH+9
5PPEGHpXhcsLFt0FX8cUd6+alCAvIM9drCqm58TS/J5w6OuZaFwK8iuyA0EILRs1
BKnxb8l+HXM+pzNB9Pz3bqwio7Hr1ugQUXATJ0L7eQKBgQD4W13gNQXhQtei1YRh
sxMs8y9GHQo+FVXm4fS/h1I4yJJ/BsUS6EkP79ZutCE1gecuRESPKx0H6sNQ7fBp
UNtroTyYDqFE20+lak+H2aTpH2SBoTYz4pl7epBAkq0pyQgEeTKtUke0tzep1I4j
ydjrH53ob2kCaetd5bB7LrVJbQKBgQDNZhPtnOxl8gkL8tuMV1MefEfHD+OeaTPL
NQsZjpUPhUgfV8S/5m4VJQC4EdBTqYx/aeIRNkxtMCqZNFIE8BUb68D9e9goJpe2
pimimGScY6SblwIL1NsZfwDq+n2hpHvfWZzUp/2nclTTGh0Z8/gjew/uDUircRzX
PSqOmUFJOwKBgDNwiSMVGGCtvYgGfWLW+lPHErWM8kAlnyMxDcZVutvz/xO8TTk7
T1azsFBBktdITp+wmBqnLV4ka8vpXHATxT6nqKs97H0ch4SVXl+e3p9CV0jaISXh
+zQuEI6vUppi/vweNjbb4eo2QJs2YTJcbkdUxxoLaU6MralHF6SL6hSJAoGBAMDh
ld8x2JDXXAV7dw5wRp6/KIxXcHGm6zttQVIrofDkQVkli56FnmR+zhEMsWyPMF/K
J7/wFI8Ih0g9aLQ4XCpPFnkznkX+D8Q2f6yvnPN7Yu21PfesWF+18z+2INn1Y1nX
hj3wz3M6G0vuHtvrTd7LxqbOlKpiWyoIx3kHk9ZXAoGBAJ1LYQQvtguUcolIqaBh
WzDKEDMV84LdNoFDNDRQ9z4Bjl5xrUSH6sqyZr2phJEE+ONVWwhsIjBIiG6faJmi
Ycny2nM93jpwamAlKjeKS5F8K5gLxoouSmp+0FkE/lURId3Av+6XoK7kMPvTJmrr
sVaOlbQnFfDO9v9eHw+E3vsz
-----END PRIVATE KEY-----"#;

    const TEST_JWKS: &str = r#"{
  "keys": [
    {
      "kty": "RSA",
      "use": "sig",
      "kid": "sm-test-key",
      "alg": "RS256",
      "n": "x0Qx6T6VZpDTqEgh6pWTH6DtzDZw3HLSiL2Az7wkR1zxg9AXMGJ9TTLtO5FbIoGqrLnRdou7nu6ofVL5wOu6x-fOzgHfeqwwbobQwORlrsnTJRnxinVc3L9h5yKktm3c4onhsRvI8zJM9AfeVzP45Wkx1pUKuTfx2flrVZFL8kaEMaDtXlNqdasmeK8gAP7IMDqsfnOcjmqj24z5eRqbwOOmgj9rNxBX5CjqTYOVVkyU9u-COZqdGtbf8lSnJBzN0dtazqxKykNrbRxA9dVuBIIollE63P4V2RD7kfzK1XcSrOp48mLfO85fqG172nOBPa0D0I6gRZdCB54VP2UBHw",
      "e": "AQAB"
    }
  ]
}"#;

    #[derive(Serialize)]
    struct TestClaims<'a> {
        sub: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        aud: Option<&'a str>,
        iss: &'a str,
        exp: usize,
        iat: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        nbf: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        common_name: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<&'a str>,
    }

    fn test_jwks() -> JwkSet {
        serde_json::from_str(TEST_JWKS).expect("jwks")
    }

    fn test_token(aud: Option<&str>, iss: &str, kid: &str) -> String {
        test_token_with_identity(
            aud,
            iss,
            kid,
            None,
            Some("sm-phone-1"),
            Some("owner@example.com"),
        )
    }

    fn test_token_with_nbf(aud: Option<&str>, iss: &str, kid: &str, nbf: Option<usize>) -> String {
        test_token_with_identity(
            aud,
            iss,
            kid,
            nbf,
            Some("sm-phone-1"),
            Some("owner@example.com"),
        )
    }

    fn test_token_with_identity(
        aud: Option<&str>,
        iss: &str,
        kid: &str,
        nbf: Option<usize>,
        common_name: Option<&str>,
        email: Option<&str>,
    ) -> String {
        test_token_with_subject_identity("user-id", aud, iss, kid, nbf, common_name, email)
    }

    fn test_token_with_subject_identity(
        sub: &str,
        aud: Option<&str>,
        iss: &str,
        kid: &str,
        nbf: Option<usize>,
        common_name: Option<&str>,
        email: Option<&str>,
    ) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_owned());
        let claims = TestClaims {
            sub,
            aud,
            iss,
            exp: 4_102_444_800,
            iat: 1_700_000_000,
            nbf,
            common_name,
            email,
        };
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).expect("private key"),
        )
        .expect("token")
    }

    fn access_config() -> CloudflareAccessConfig {
        CloudflareAccessConfig {
            team_domain: Some("team.cloudflareaccess.com".to_owned()),
            browser: CloudflareAccessApplicationConfig {
                enabled: true,
                hostname: Some("sm.example.com".to_owned()),
                jwt_audience: Some("sm-browser-aud".to_owned()),
                ..CloudflareAccessApplicationConfig::default()
            },
            mobile_app: CloudflareAccessApplicationConfig {
                enabled: true,
                hostname: Some("sm-app.example.com".to_owned()),
                jwt_audience: Some("sm-mobile-aud".to_owned()),
                ..CloudflareAccessApplicationConfig::default()
            },
            node_fallback: CloudflareAccessApplicationConfig {
                enabled: true,
                hostname: Some("sm-node.example.com".to_owned()),
                jwt_audience: Some("sm-node-aud".to_owned()),
                ..CloudflareAccessApplicationConfig::default()
            },
            email_worker: CloudflareAccessApplicationConfig {
                enabled: true,
                hostname: Some("sm-email.example.com".to_owned()),
                jwt_audience: Some("sm-email-aud".to_owned()),
                ..CloudflareAccessApplicationConfig::default()
            },
            ..CloudflareAccessConfig::default()
        }
    }

    #[test]
    fn verifies_valid_cloudflare_access_assertion() {
        let token = test_token(
            Some("sm-mobile-aud"),
            "https://team.cloudflareaccess.com",
            "sm-test-key",
        );
        let claims = verify_cloudflare_access_assertion_with_jwks(
            &token,
            "https://team.cloudflareaccess.com",
            "sm-mobile-aud",
            &test_jwks(),
        )
        .expect("valid token");

        assert_eq!(claims.sub, "user-id");
        assert_eq!(claims.common_name.as_deref(), Some("sm-phone-1"));
        assert_eq!(claims.email.as_deref(), Some("owner@example.com"));
    }

    #[test]
    fn maps_configured_hosts_to_access_applications() {
        let config = access_config();
        assert_eq!(
            cloudflare_access_application_for_host(&config, "sm.example.com"),
            Some(CloudflareAccessApplication::Browser)
        );
        assert_eq!(
            cloudflare_access_application_for_host(&config, "SM-APP.EXAMPLE.COM:443"),
            Some(CloudflareAccessApplication::MobileApp)
        );
        assert_eq!(
            cloudflare_access_application_for_host(&config, "sm-node.example.com"),
            Some(CloudflareAccessApplication::NodeFallback)
        );
        assert_eq!(
            cloudflare_access_application_for_host(&config, "sm-email.example.com"),
            Some(CloudflareAccessApplication::EmailWorker)
        );
        assert_eq!(
            cloudflare_access_application_for_host(&config, "unknown.example.com"),
            None
        );
    }

    #[test]
    fn detects_enabled_apps_without_configured_hostname() {
        let mut config = access_config();
        assert!(!cloudflare_access_has_enabled_app_without_hostname(&config));

        config.mobile_app.hostname = Some(" ".to_owned());
        assert!(cloudflare_access_has_enabled_app_without_hostname(&config));

        config.mobile_app.enabled = false;
        assert!(!cloudflare_access_has_enabled_app_without_hostname(&config));
    }

    #[test]
    fn classifies_browser_mobile_node_and_worker_contexts() {
        let config = access_config();
        let jwks = test_jwks();

        let browser = classify_cloudflare_access_assertion_with_jwks(
            &config,
            CloudflareAccessApplication::Browser,
            &test_token_with_identity(
                Some("sm-browser-aud"),
                "https://team.cloudflareaccess.com",
                "sm-test-key",
                None,
                None,
                Some("owner@example.com"),
            ),
            &jwks,
        )
        .expect("browser context");
        assert_eq!(browser.application, CloudflareAccessApplication::Browser);
        assert_eq!(browser.identity, "owner@example.com");
        assert_eq!(browser.email.as_deref(), Some("owner@example.com"));

        let mobile = classify_cloudflare_access_assertion_with_jwks(
            &config,
            CloudflareAccessApplication::MobileApp,
            &test_token_with_identity(
                Some("sm-mobile-aud"),
                "https://team.cloudflareaccess.com",
                "sm-test-key",
                None,
                Some("sm-phone-1"),
                Some("owner@example.com"),
            ),
            &jwks,
        )
        .expect("mobile context");
        assert_eq!(mobile.application, CloudflareAccessApplication::MobileApp);
        assert_eq!(mobile.identity, "sm-phone-1");
        assert_eq!(mobile.common_name.as_deref(), Some("sm-phone-1"));

        let node = classify_cloudflare_access_assertion_with_jwks(
            &config,
            CloudflareAccessApplication::NodeFallback,
            &test_token_with_identity(
                Some("sm-node-aud"),
                "https://team.cloudflareaccess.com",
                "sm-test-key",
                None,
                Some("macbook-node"),
                None,
            ),
            &jwks,
        )
        .expect("node context");
        assert_eq!(node.application, CloudflareAccessApplication::NodeFallback);
        assert_eq!(node.identity, "macbook-node");

        let worker = classify_cloudflare_access_assertion_with_jwks(
            &config,
            CloudflareAccessApplication::EmailWorker,
            &test_token_with_identity(
                Some("sm-email-aud"),
                "https://team.cloudflareaccess.com",
                "sm-test-key",
                None,
                None,
                None,
            ),
            &jwks,
        )
        .expect("worker context");
        assert_eq!(worker.application, CloudflareAccessApplication::EmailWorker);
        assert_eq!(worker.identity, "user-id");
    }

    #[test]
    fn email_worker_context_accepts_common_name_without_subject() {
        let context = classify_cloudflare_access_assertion_with_jwks(
            &access_config(),
            CloudflareAccessApplication::EmailWorker,
            &test_token_with_subject_identity(
                "",
                Some("sm-email-aud"),
                "https://team.cloudflareaccess.com",
                "sm-test-key",
                None,
                Some("sm-email-worker"),
                None,
            ),
            &test_jwks(),
        )
        .expect("worker context");

        assert_eq!(
            context.application,
            CloudflareAccessApplication::EmailWorker
        );
        assert_eq!(context.subject, "");
        assert_eq!(context.identity, "sm-email-worker");
        assert_eq!(context.common_name.as_deref(), Some("sm-email-worker"));
    }

    #[test]
    fn context_classification_is_audience_bound() {
        let config = access_config();
        let token = test_token_with_identity(
            Some("sm-browser-aud"),
            "https://team.cloudflareaccess.com",
            "sm-test-key",
            None,
            Some("sm-phone-1"),
            Some("owner@example.com"),
        );

        assert_eq!(
            classify_cloudflare_access_assertion_with_jwks(
                &config,
                CloudflareAccessApplication::MobileApp,
                &token,
                &test_jwks()
            ),
            Err(CloudflareAccessContextError::InvalidAssertion(
                CloudflareAccessError::InvalidAssertion
            ))
        );
    }

    #[test]
    fn context_classification_requires_expected_identity_claims() {
        let config = access_config();
        let jwks = test_jwks();

        assert_eq!(
            classify_cloudflare_access_assertion_with_jwks(
                &config,
                CloudflareAccessApplication::Browser,
                &test_token_with_identity(
                    Some("sm-browser-aud"),
                    "https://team.cloudflareaccess.com",
                    "sm-test-key",
                    None,
                    Some("sm-phone-1"),
                    None,
                ),
                &jwks
            ),
            Err(CloudflareAccessContextError::MissingIdentity)
        );
        assert_eq!(
            classify_cloudflare_access_assertion_with_jwks(
                &config,
                CloudflareAccessApplication::MobileApp,
                &test_token_with_identity(
                    Some("sm-mobile-aud"),
                    "https://team.cloudflareaccess.com",
                    "sm-test-key",
                    None,
                    None,
                    Some("owner@example.com"),
                ),
                &jwks
            ),
            Err(CloudflareAccessContextError::MissingIdentity)
        );
    }

    #[test]
    fn context_classification_fails_closed_for_disabled_or_incomplete_apps() {
        let mut config = access_config();
        config.mobile_app.enabled = false;
        assert_eq!(
            classify_cloudflare_access_assertion_with_jwks(
                &config,
                CloudflareAccessApplication::MobileApp,
                &test_token(
                    Some("sm-mobile-aud"),
                    "https://team.cloudflareaccess.com",
                    "sm-test-key"
                ),
                &test_jwks()
            ),
            Err(CloudflareAccessContextError::Disabled)
        );

        let mut config = access_config();
        config.team_domain = None;
        assert_eq!(
            classify_cloudflare_access_assertion_with_jwks(
                &config,
                CloudflareAccessApplication::MobileApp,
                &test_token(
                    Some("sm-mobile-aud"),
                    "https://team.cloudflareaccess.com",
                    "sm-test-key"
                ),
                &test_jwks()
            ),
            Err(CloudflareAccessContextError::IncompleteConfig)
        );

        let mut config = access_config();
        config.mobile_app.jwt_audience = None;
        assert_eq!(
            classify_cloudflare_access_assertion_with_jwks(
                &config,
                CloudflareAccessApplication::MobileApp,
                &test_token(
                    Some("sm-mobile-aud"),
                    "https://team.cloudflareaccess.com",
                    "sm-test-key"
                ),
                &test_jwks()
            ),
            Err(CloudflareAccessContextError::IncompleteConfig)
        );
    }

    #[test]
    fn rejects_missing_audience_config() {
        let token = test_token(
            Some("sm-mobile-aud"),
            "https://team.cloudflareaccess.com",
            "sm-test-key",
        );
        assert_eq!(
            verify_cloudflare_access_assertion_with_jwks(
                &token,
                "https://team.cloudflareaccess.com",
                " ",
                &test_jwks()
            ),
            Err(CloudflareAccessError::MissingAudience)
        );
    }

    #[test]
    fn rejects_malformed_token_before_jwks_fetch() {
        assert_eq!(
            verify_cloudflare_access_assertion(
                "not-a-jwt",
                "https://team.cloudflareaccess.com",
                "sm-mobile-aud",
            ),
            Err(CloudflareAccessError::MalformedToken)
        );
    }

    #[test]
    fn rejects_missing_issuer_config() {
        let token = test_token(
            Some("sm-mobile-aud"),
            "https://team.cloudflareaccess.com",
            "sm-test-key",
        );
        assert_eq!(
            verify_cloudflare_access_assertion_with_jwks(
                &token,
                " ",
                "sm-mobile-aud",
                &test_jwks()
            ),
            Err(CloudflareAccessError::MissingIssuer)
        );
    }

    #[test]
    fn rejects_invalid_expected_issuer_config() {
        let token = test_token(
            Some("sm-mobile-aud"),
            "https://team.cloudflareaccess.com",
            "sm-test-key",
        );
        assert_eq!(
            verify_cloudflare_access_assertion_with_jwks(
                &token,
                "https://example.com",
                "sm-mobile-aud",
                &test_jwks()
            ),
            Err(CloudflareAccessError::InvalidIssuer)
        );
    }

    #[test]
    fn rejects_wrong_audience() {
        let token = test_token(
            Some("sm-mobile-aud"),
            "https://team.cloudflareaccess.com",
            "sm-test-key",
        );
        assert_eq!(
            verify_cloudflare_access_assertion_with_jwks(
                &token,
                "https://team.cloudflareaccess.com",
                "other-aud",
                &test_jwks()
            ),
            Err(CloudflareAccessError::InvalidAssertion)
        );
    }

    #[test]
    fn rejects_missing_audience_claim() {
        let token = test_token(None, "https://team.cloudflareaccess.com", "sm-test-key");
        assert_eq!(
            verify_cloudflare_access_assertion_with_jwks(
                &token,
                "https://team.cloudflareaccess.com",
                "sm-mobile-aud",
                &test_jwks()
            ),
            Err(CloudflareAccessError::InvalidAssertion)
        );
    }

    #[test]
    fn rejects_unknown_key_id() {
        let token = test_token(
            Some("sm-mobile-aud"),
            "https://team.cloudflareaccess.com",
            "other-key",
        );
        assert_eq!(
            verify_cloudflare_access_assertion_with_jwks(
                &token,
                "https://team.cloudflareaccess.com",
                "sm-mobile-aud",
                &test_jwks()
            ),
            Err(CloudflareAccessError::UnknownKeyId)
        );
    }

    #[test]
    fn rejects_wrong_cloudflare_team_issuer() {
        let token = test_token(
            Some("sm-mobile-aud"),
            "https://other.cloudflareaccess.com",
            "sm-test-key",
        );
        assert_eq!(
            verify_cloudflare_access_assertion_with_jwks(
                &token,
                "https://team.cloudflareaccess.com",
                "sm-mobile-aud",
                &test_jwks()
            ),
            Err(CloudflareAccessError::InvalidAssertion)
        );
    }

    #[test]
    fn rejects_token_before_not_before_time() {
        let token = test_token_with_nbf(
            Some("sm-mobile-aud"),
            "https://team.cloudflareaccess.com",
            "sm-test-key",
            Some(4_102_444_700),
        );
        assert_eq!(
            verify_cloudflare_access_assertion_with_jwks(
                &token,
                "https://team.cloudflareaccess.com",
                "sm-mobile-aud",
                &test_jwks()
            ),
            Err(CloudflareAccessError::InvalidAssertion)
        );
    }

    #[test]
    fn rejects_malformed_token() {
        assert_eq!(
            verify_cloudflare_access_assertion_with_jwks(
                "not-a-jwt",
                "https://team.cloudflareaccess.com",
                "sm-mobile-aud",
                &test_jwks()
            ),
            Err(CloudflareAccessError::MalformedToken)
        );
    }

    #[test]
    fn issuer_guard_accepts_only_cloudflare_access_https_hosts() {
        assert_eq!(
            cloudflare_access_issuer("https://team.cloudflareaccess.com"),
            Some("https://team.cloudflareaccess.com".to_owned())
        );
        assert_eq!(
            cloudflare_access_issuer("https://rajeshgoli.cloudflareaccess.com/"),
            Some("https://rajeshgoli.cloudflareaccess.com".to_owned())
        );
        assert!(cloudflare_access_issuer("http://rajeshgoli.cloudflareaccess.com").is_none());
        assert!(cloudflare_access_issuer("https://cloudflareaccess.com.evil.test").is_none());
        assert!(cloudflare_access_issuer("https://example.test").is_none());
        assert!(cloudflare_access_issuer("https://team.cloudflareaccess.com/path").is_none());
    }
}
