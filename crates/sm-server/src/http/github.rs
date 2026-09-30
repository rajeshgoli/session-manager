//! Owner-only GitHub issue and pull request reading for the reading pane.

use super::*;

type Cache = BTreeMap<(String, i64), (Instant, Value)>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn markdown_html(source: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser};
    let events = Parser::new_ext(source, Options::all())
        .filter(|event| !matches!(event, Event::Html(_) | Event::InlineHtml(_)));
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events);
    html
}

fn github_error(error: String) -> ApiError {
    ApiError::Status {
        status: StatusCode::BAD_GATEWAY,
        detail: error,
    }
}

fn read_item(repo: &str, number: i64) -> Result<Value, ApiError> {
    let key = (repo.to_owned(), number);
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Ok(cache) = cache.lock() {
        if let Some((at, value)) = cache.get(&key) {
            if at.elapsed() < Duration::from_secs(60) {
                return Ok(value.clone());
            }
        }
    }
    let issue = gh_api_json(repo, &format!("issues/{number}"), false).map_err(github_error)?;
    let comments = gh_api_json(
        repo,
        &format!("issues/{number}/comments?per_page=100"),
        false,
    )
    .map_err(github_error)?;
    let is_pr = issue["pull_request"].is_object();
    let pr = if is_pr {
        let pull = gh_api_json(repo, &format!("pulls/{number}"), false).map_err(github_error)?;
        let decision_args = vec![
            "pr".to_owned(),
            "view".to_owned(),
            number.to_string(),
            "-R".to_owned(),
            repo.to_owned(),
            "--json".to_owned(),
            "reviewDecision".to_owned(),
        ];
        let decision = gh_command_output(&decision_args, Duration::from_secs(30))
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
            .map(|value| value["reviewDecision"].clone())
            .unwrap_or(Value::Null);
        let sha = pull["head"]["sha"].as_str().unwrap_or_default();
        let checks = if sha.is_empty() {
            Value::Null
        } else {
            gh_api_json(
                repo,
                &format!("commits/{sha}/check-runs?per_page=100"),
                false,
            )
            .map_err(github_error)?
        };
        let checks = checks["check_runs"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|check| {
                        json!({
                            "name": check["name"], "conclusion": check["conclusion"],
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        json!({
            "head": pull["head"]["ref"], "base": pull["base"]["ref"],
            "draft": pull["draft"], "merged": pull["merged"],
            "mergeable": pull["mergeable"], "review_decision": decision,
            "checks": checks,
        })
    } else {
        Value::Null
    };
    let comments =
        comments
            .as_array()
            .map(|items| {
                items.iter().map(|comment| json!({
        "author": comment["user"]["login"], "created_at": comment["created_at"],
        "body_html": markdown_html(comment["body"].as_str().unwrap_or_default()),
    })).collect::<Vec<_>>()
            })
            .unwrap_or_default();
    let value = json!({
        "kind": if is_pr { "pr" } else { "issue" }, "number": number,
        "title": issue["title"], "state": issue["state"],
        "state_reason": issue["state_reason"], "author": issue["user"]["login"],
        "created_at": issue["created_at"], "url": issue["html_url"],
        "labels": issue["labels"].as_array().map(|labels| labels.iter().map(|label| label["name"].clone()).collect::<Vec<_>>()).unwrap_or_default(),
        "body_html": markdown_html(issue["body"].as_str().unwrap_or_default()),
        "comments": comments, "pr": pr,
    });
    if let Ok(mut cache) = cache.lock() {
        cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(60));
        cache.insert(key, (Instant::now(), value.clone()));
    }
    Ok(value)
}

pub(super) async fn get_item(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Result<Json<Value>, ApiError> {
    super::board::owner_guard(&state, &headers, peer_addr, "GET", &uri, false)?;
    let repo = format!("{owner}/{name}");
    crate::owner_docs::validate_repo_slug(&repo).map_err(|error| ApiError::Status {
        status: StatusCode::BAD_REQUEST,
        detail: error.to_string(),
    })?;
    if number <= 0 {
        return Err(ApiError::NotFound("GitHub item not found"));
    }
    Ok(Json(
        tokio::task::spawn_blocking(move || read_item(&repo, number))
            .await
            .map_err(|error| {
                ApiError::Internal(anyhow::anyhow!("GitHub read failed: {error}"))
            })??,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_markdown_strips_raw_html() {
        let html = markdown_html("hello <script>alert(1)</script> **world**");
        assert!(!html.contains("<script>"));
        assert!(html.contains("<strong>world</strong>"));
    }
}
