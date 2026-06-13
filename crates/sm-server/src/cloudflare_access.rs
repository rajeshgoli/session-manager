use axum::http::Uri;
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

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

    let header = decode_header(token).map_err(|_| CloudflareAccessError::MalformedToken)?;
    let kid = header
        .kid
        .as_deref()
        .ok_or(CloudflareAccessError::MissingKeyId)?;
    let key = DecodingKey::from_jwk(jwks.find(kid).ok_or(CloudflareAccessError::UnknownKeyId)?)
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
    verify_cloudflare_access_assertion_with_jwks(token, &expected_issuer, expected_audience, &jwks)
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
        common_name: &'a str,
        email: &'a str,
    }

    fn test_jwks() -> JwkSet {
        serde_json::from_str(TEST_JWKS).expect("jwks")
    }

    fn test_token(aud: Option<&str>, iss: &str, kid: &str) -> String {
        test_token_with_nbf(aud, iss, kid, None)
    }

    fn test_token_with_nbf(aud: Option<&str>, iss: &str, kid: &str, nbf: Option<usize>) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_owned());
        let claims = TestClaims {
            sub: "user-id",
            aud,
            iss,
            exp: 4_102_444_800,
            iat: 1_700_000_000,
            nbf,
            common_name: "sm-phone-1",
            email: "owner@example.com",
        };
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).expect("private key"),
        )
        .expect("token")
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
