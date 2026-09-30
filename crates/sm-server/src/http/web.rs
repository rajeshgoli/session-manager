//! The web app (spec 1710 D1, D2): on the browser hostname, every page path
//! serves one HTML shell that loads the app's ES modules from `/assets/`,
//! and the app routes on the client. Every other hostname keeps today's
//! page at each path, and JSON responses are unchanged everywhere.
//!
//! Page paths join the shell as their page modules land: `/` and `/watch`
//! (Agents), `/board`, `/queue`, `/analytics…`, `/settings` and `/terminal/{id}` here;
//! `/inbox`, `/history…` and `/guestbook` keep today's page until
//! the tickets that build those modules route them through [`shell_page`].

use super::*;
use crate::owner_doc_render::inline_json;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

struct Asset {
    name: &'static str,
    body: &'static str,
    content_type: &'static str,
}

macro_rules! asset {
    ($name:literal, $content_type:expr) => {
        Asset {
            name: $name,
            body: include_str!(concat!("../web/", $name)),
            content_type: $content_type,
        }
    };
}

const JS: &str = "text/javascript; charset=utf-8";

/// The web app's files, served at `/assets/{name}`.
const ASSETS: &[Asset] = &[
    asset!("app.css", "text/css; charset=utf-8"),
    asset!("app.js", JS),
    asset!("ui.js", JS),
    asset!("start.js", JS),
    asset!("agents.js", JS),
    asset!("queue.js", JS),
    asset!("queue-model.js", JS),
    asset!("analytics.js", JS),
    asset!("queue.css", "text/css; charset=utf-8"),
    asset!("settings.js", JS),
    asset!("terminal.js", JS),
    asset!("vendor/xterm.js", JS),
    asset!("vendor/addon-fit.js", JS),
    asset!("vendor/xterm.css", "text/css; charset=utf-8"),
    asset!("board.js", JS),
    asset!("board-start.js", JS),
    asset!("vendor/preact.module.js", JS),
    asset!("vendor/hooks.module.js", JS),
    asset!("vendor/htm.module.js", JS),
];

/// Bare module names the app imports, mapped to their vendored files.
const VENDOR_IMPORTS: &[(&str, &str)] = &[
    ("preact", "vendor/preact.module.js"),
    ("preact/hooks", "vendor/hooks.module.js"),
    ("htm", "vendor/htm.module.js"),
];

/// A hash of every embedded file. Asset URLs carry it as `?v=`, so a new
/// build is a new URL and the year-long cache never serves stale code.
pub(super) fn build_id() -> &'static str {
    static BUILD_ID: OnceLock<String> = OnceLock::new();
    BUILD_ID.get_or_init(|| {
        let mut hasher = Sha256::new();
        for asset in ASSETS {
            hasher.update(asset.name.as_bytes());
            hasher.update([0]);
            hasher.update(asset.body.as_bytes());
            hasher.update([0]);
        }
        hasher
            .finalize()
            .iter()
            .take(6)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    })
}

fn wants_json(request: &Request) -> bool {
    request
        .uri()
        .query()
        .is_some_and(|query| query.split('&').any(|pair| pair == "format=json"))
        || request
            .headers()
            .get("accept")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("application/json"))
}

/// D1's hostname rule: the browser hostname, or a loopback request with no
/// Access headers, gets the shell. JSON requests never do.
pub(super) fn wants_shell(state: &AppState, request: &Request) -> bool {
    if wants_json(request) {
        return false;
    }
    request_cloudflare_access_application(state, request)
        == Some(CloudflareAccessApplication::Browser)
        || (is_request_local_bypass(state, request)
            && header_text(request.headers(), "cf-access-jwt-assertion").is_none())
}

/// The shell document, when this request should get it. Callers run the
/// page's read guard first.
pub(super) fn shell_page(state: &AppState, request: &Request) -> Option<Response> {
    wants_shell(state, request).then(|| shell_response(state))
}

fn shell_response(state: &AppState) -> Response {
    let id = build_id();
    let mut imports = serde_json::Map::new();
    for (name, file) in VENDOR_IMPORTS {
        imports.insert((*name).to_owned(), json!(format!("/assets/{file}?v={id}")));
    }
    // Relative imports between the app's own modules resolve to
    // `/assets/{name}`; mapping those URLs adds the version.
    for asset in ASSETS.iter().filter(|asset| asset.content_type == JS) {
        imports.insert(
            format!("/assets/{}", asset.name),
            json!(format!("/assets/{}?v={id}", asset.name)),
        );
    }
    let config = json!({
        "build_id": id,
        "server_version": env!("CARGO_PKG_VERSION"),
        "queue_config_limits": {
            "max_running": state.config.queue_admission_policy().max_running_jobs,
            "tests": state.config.queue_admission_policy().tests_max_concurrent,
            "perf": state.config.queue_admission_policy().perf_max_concurrent,
            "background": state.config.queue_admission_policy().background_max_concurrent,
            "service": state.config.queue_admission_policy().service_max_concurrent,
        },
        "refresh_seconds": state.config.web_watch.refresh_seconds(),
        "stall_minutes": state.config.board.stall_minutes,
        "owner_name": state.config.owner_name,
        // Owner messages from the agent panel use the inbox's page token.
        "inbox_token": docs::issue_doc_token(&state.config, "inbox"),
    });
    let html = format!(
        r#"<!doctype html>
<html lang="en" data-theme="system">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>sm</title>
<script>try{{var t=localStorage.getItem("sm-theme");if(t==="light"||t==="dark")document.documentElement.dataset.theme=t}}catch(e){{}}</script>
<link rel="stylesheet" href="/assets/vendor/xterm.css?v={id}">
<link rel="stylesheet" href="/assets/app.css?v={id}">
<link rel="stylesheet" href="/assets/queue.css?v={id}">
<script type="importmap">{imports}</script>
<script type="application/json" id="sm-config">{config}</script>
<script type="module" src="/assets/app.js?v={id}"></script>
</head>
<body><div id="app"></div></body>
</html>
"#,
        imports = inline_json(&json!({ "imports": imports })),
        config = inline_json(&config),
    );
    super::history::html_response(StatusCode::OK, html)
}

/// `GET /queue`, `/analytics…`, `/settings`, `/terminal/{id}`: shell-only
/// paths, with no page on any other hostname.
pub(super) async fn get_shell_only_page(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    shell_page(&state, &request).ok_or(ApiError::NotFound("Not found"))
}

/// `GET /assets/{name}`: a web app file, cached for a year because its URL
/// carries the build id.
pub(super) async fn get_asset(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    let asset = ASSETS
        .iter()
        .find(|asset| asset.name == name)
        .ok_or(ApiError::NotFound("Not found"))?;
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, asset.content_type.to_owned()),
            (
                CACHE_CONTROL,
                "public, max-age=31536000, immutable".to_owned(),
            ),
        ],
        asset.body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_javascript_stays_under_the_size_budget() {
        let own: usize = ASSETS
            .iter()
            .filter(|asset| asset.content_type == JS && !asset.name.starts_with("vendor/"))
            .map(|asset| asset.body.len())
            .sum();
        assert!(own <= 200 * 1024, "own JS is {own} bytes");
    }

    #[test]
    fn every_vendor_import_is_served() {
        for (_, file) in VENDOR_IMPORTS {
            assert!(ASSETS.iter().any(|asset| asset.name == *file), "{file}");
        }
    }

    #[test]
    fn modules_import_only_mapped_names() {
        for asset in ASSETS.iter().filter(|asset| asset.content_type == JS) {
            for line in asset.body.lines() {
                let Some(rest) = line.split(" from ").nth(1) else {
                    continue;
                };
                let Some(spec) = rest.trim().trim_end_matches(';').strip_prefix('\'') else {
                    continue;
                };
                let spec = spec.trim_end_matches('\'');
                let known = VENDOR_IMPORTS.iter().any(|(name, _)| *name == spec)
                    || spec
                        .strip_prefix("./")
                        .is_some_and(|file| ASSETS.iter().any(|asset| asset.name == file));
                assert!(known, "{}: {spec}", asset.name);
            }
        }
    }
}
