use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::{de, Deserialize, Deserializer};
use time::OffsetDateTime;

const GOOGLE_ID_TOKEN_JWKS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";

#[derive(Debug, Clone, Deserialize)]
pub struct GoogleIdTokenClaims {
    pub aud: String,
    pub exp: usize,
    pub iat: usize,
    pub iss: String,
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, deserialize_with = "deserialize_email_verified")]
    pub email_verified: bool,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoogleIdTokenError {
    MalformedToken,
    MissingKeyId,
    JwksFetchFailed,
    UnknownKeyId,
    InvalidToken,
}

pub fn fetch_google_id_token_jwks() -> Result<JwkSet, GoogleIdTokenError> {
    let mut response = ureq::get(GOOGLE_ID_TOKEN_JWKS_URL)
        .call()
        .map_err(|_| GoogleIdTokenError::JwksFetchFailed)?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|_| GoogleIdTokenError::JwksFetchFailed)?;
    serde_json::from_str(&body).map_err(|_| GoogleIdTokenError::JwksFetchFailed)
}

pub fn google_id_token_key_id(token: &str) -> Result<String, GoogleIdTokenError> {
    let header = decode_header(token).map_err(|_| GoogleIdTokenError::MalformedToken)?;
    header
        .kid
        .filter(|kid| !kid.trim().is_empty())
        .ok_or(GoogleIdTokenError::MissingKeyId)
}

pub fn verify_google_id_token_with_jwks(
    token: &str,
    jwks: &JwkSet,
) -> Result<GoogleIdTokenClaims, GoogleIdTokenError> {
    let kid = google_id_token_key_id(token)?;
    let key = DecodingKey::from_jwk(jwks.find(&kid).ok_or(GoogleIdTokenError::UnknownKeyId)?)
        .map_err(|_| GoogleIdTokenError::InvalidToken)?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&["accounts.google.com", "https://accounts.google.com"]);
    validation.set_required_spec_claims(&["exp", "iss", "aud"]);
    validation.leeway = 0;
    validation.validate_aud = false;
    let claims = decode::<GoogleIdTokenClaims>(token, &key, &validation)
        .map(|decoded| decoded.claims)
        .map_err(|_| GoogleIdTokenError::InvalidToken)?;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let iat = i64::try_from(claims.iat).map_err(|_| GoogleIdTokenError::InvalidToken)?;
    if iat > now {
        return Err(GoogleIdTokenError::InvalidToken);
    }
    Ok(claims)
}

fn deserialize_email_verified<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(serde_json::Value::Bool(value)) => Ok(value),
        Some(serde_json::Value::String(value)) => Ok(value.trim().eq_ignore_ascii_case("true")),
        Some(other) => Err(de::Error::custom(format!(
            "invalid email_verified value: {other}"
        ))),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
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
      "kid": "google-test-key",
      "alg": "RS256",
      "n": "x0Qx6T6VZpDTqEgh6pWTH6DtzDZw3HLSiL2Az7wkR1zxg9AXMGJ9TTLtO5FbIoGqrLnRdou7nu6ofVL5wOu6x-fOzgHfeqwwbobQwORlrsnTJRnxinVc3L9h5yKktm3c4onhsRvI8zJM9AfeVzP45Wkx1pUKuTfx2flrVZFL8kaEMaDtXlNqdasmeK8gAP7IMDqsfnOcjmqj24z5eRqbwOOmgj9rNxBX5CjqTYOVVkyU9u-COZqdGtbf8lSnJBzN0dtazqxKykNrbRxA9dVuBIIollE63P4V2RD7kfzK1XcSrOp48mLfO85fqG172nOBPa0D0I6gRZdCB54VP2UBHw",
      "e": "AQAB"
    }
  ]
}"#;

    #[derive(Serialize)]
    struct TestGoogleClaims<'a> {
        sub: &'a str,
        aud: &'a str,
        iss: &'a str,
        exp: usize,
        iat: usize,
        email: &'a str,
        email_verified: bool,
        name: &'a str,
    }

    pub(crate) fn test_google_jwks() -> JwkSet {
        serde_json::from_str(TEST_JWKS).expect("jwks")
    }

    pub(crate) fn test_private_key_pem() -> &'static [u8] {
        TEST_PRIVATE_KEY.as_bytes()
    }

    pub(crate) fn test_google_id_token(
        audience: &str,
        email: &str,
        email_verified: bool,
        name: &str,
    ) -> String {
        test_google_id_token_with_times(
            audience,
            email,
            email_verified,
            name,
            1_700_000_000,
            4_102_444_800,
        )
    }

    pub(crate) fn test_google_id_token_with_times(
        audience: &str,
        email: &str,
        email_verified: bool,
        name: &str,
        iat: usize,
        exp: usize,
    ) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("google-test-key".to_owned());
        let claims = TestGoogleClaims {
            sub: "google-user-id",
            aud: audience,
            iss: "https://accounts.google.com",
            exp,
            iat,
            email,
            email_verified,
            name,
        };
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).expect("private key"),
        )
        .expect("token")
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::*, *};

    #[test]
    fn rejects_future_iat_and_just_expired_token() {
        let jwks = test_google_jwks();
        let now = OffsetDateTime::now_utc().unix_timestamp() as usize;
        let future_iat = test_google_id_token_with_times(
            "android-client-id",
            "rajeshgoli@gmail.com",
            true,
            "Rajesh Goli",
            now + 3600,
            now + 7200,
        );
        assert_eq!(
            verify_google_id_token_with_jwks(&future_iat, &jwks).unwrap_err(),
            GoogleIdTokenError::InvalidToken
        );

        let just_expired = test_google_id_token_with_times(
            "android-client-id",
            "rajeshgoli@gmail.com",
            true,
            "Rajesh Goli",
            now - 3600,
            now - 30,
        );
        assert_eq!(
            verify_google_id_token_with_jwks(&just_expired, &jwks).unwrap_err(),
            GoogleIdTokenError::InvalidToken
        );
    }
}
