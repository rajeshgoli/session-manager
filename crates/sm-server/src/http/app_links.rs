//! `/.well-known/assetlinks.json` (#2594): the one public file on the owner's
//! browser host. Android's App Links check reads it, without signing in, to
//! confirm the sm app may open that host's reader links. It names the package
//! and its signing certificate and grants link handling only, never credential
//! sharing (`get_login_creds`).

use super::*;
use crate::config::AndroidAppLinksConfig;

pub(super) async fn assetlinks(State(state): State<Arc<AppState>>) -> Response {
    match statement(&state.config.android_app_links) {
        Some(body) => ([(CONTENT_TYPE, "application/json")], body.to_string()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The statement list, or None when the package or every fingerprint is missing or malformed.
fn statement(config: &AndroidAppLinksConfig) -> Option<Value> {
    let package = config.package_name.trim();
    let valid_package = package.split('.').count() >= 2
        && package.split('.').all(|part| {
            part.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    let fingerprints: Vec<String> = config
        .sha256_cert_fingerprints
        .iter()
        .filter_map(|value| fingerprint(value))
        .collect();
    if !valid_package || fingerprints.is_empty() {
        return None;
    }
    Some(json!([{
        "relation": ["delegate_permission/common.handle_all_urls"],
        "target": {
            "namespace": "android_app",
            "package_name": package,
            "sha256_cert_fingerprints": fingerprints,
        },
    }]))
}

/// `AB:CD:…` in upper case, from 64 hex digits with or without colons.
fn fingerprint(value: &str) -> Option<String> {
    let hex: String = value.trim().chars().filter(|c| *c != ':').collect();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let upper = hex.to_ascii_uppercase();
    Some(
        upper
            .as_bytes()
            .chunks(2)
            .map(|pair| std::str::from_utf8(pair).unwrap())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "fdbf0bbd96cb4f42912a78edb94bca7c4fc10b30302f611731a2bfa00300f6bf";

    fn config(package: &str, fingerprints: &[&str]) -> AndroidAppLinksConfig {
        AndroidAppLinksConfig {
            package_name: package.to_owned(),
            sha256_cert_fingerprints: fingerprints.iter().map(|f| (*f).to_owned()).collect(),
        }
    }

    #[test]
    fn statement_grants_link_handling_to_the_package_and_certificate() {
        let body = statement(&config("li.rajeshgo.sm", &[DIGEST])).unwrap();
        assert_eq!(
            body,
            json!([{
                "relation": ["delegate_permission/common.handle_all_urls"],
                "target": {
                    "namespace": "android_app",
                    "package_name": "li.rajeshgo.sm",
                    "sha256_cert_fingerprints": ["FD:BF:0B:BD:96:CB:4F:42:91:2A:78:ED:B9:4B:CA:7C:4F:C1:0B:30:30:2F:61:17:31:A2:BF:A0:03:00:F6:BF"],
                },
            }])
        );
    }

    #[test]
    fn statement_accepts_colon_form_and_drops_malformed_fingerprints() {
        let colons = fingerprint(DIGEST).unwrap();
        let body = statement(&config(
            "li.rajeshgo.sm",
            &[&colons, "not-a-digest", &DIGEST[..62]],
        ))
        .unwrap();
        assert_eq!(
            body[0]["target"]["sha256_cert_fingerprints"],
            json!([colons])
        );
    }

    #[test]
    fn statement_is_absent_until_configured() {
        assert!(statement(&AndroidAppLinksConfig::default()).is_none());
        assert!(statement(&config("li.rajeshgo.sm", &[])).is_none());
        assert!(statement(&config("li.rajeshgo.sm", &["zz"])).is_none());
        assert!(statement(&config("sm", &[DIGEST])).is_none());
        assert!(statement(&config("li.rajeshgo.sm\"}", &[DIGEST])).is_none());
    }
}
