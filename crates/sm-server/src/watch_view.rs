//! What `sm watch` shows, shared by the terminal view (`bin/watch`) and the
//! web watch (`GET /watch/state`, sm#1452 ticket #1489): which sessions a
//! filter keeps, the order and depth of the session tree, a session's
//! displayed state, and the collapsed row's docs and claim markers. Both
//! read the same `/sessions` and `/session-obligations` JSON shapes, so the
//! functions take `serde_json::Value`.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

/// Friendly name, else name, else id.
pub fn name(v: &Value) -> &str {
    ["friendly_name", "name", "id"]
        .into_iter()
        .map(|k| s(v, k))
        .find(|v| !v.is_empty())
        .unwrap_or("?")
}

pub fn array(v: &Value, key: &str) -> Vec<Value> {
    v[key].as_array().cloned().unwrap_or_default()
}

/// The grouping key: the session's working directory.
pub fn repo(v: &Value) -> &str {
    let path = s(v, "working_dir");
    if path.is_empty() {
        "unknown"
    } else {
        path
    }
}

/// `activity_state`, except an idle agent with something in `waiting_on`
/// is `waiting`.
pub fn display_state<'a>(session: &'a Value, obligation: Option<&Value>) -> &'a str {
    let waiting = obligation.is_some_and(|o| !array(o, "waiting_on").is_empty());
    if s(session, "activity_state") == "idle" && waiting {
        "waiting"
    } else {
        s(session, "activity_state")
    }
}

/// Docs the owner hasn't read yet, or that await the owner's review.
pub fn unread_doc_count(obligation: &Value) -> usize {
    array(obligation, "docs")
        .iter()
        .filter(|doc| matches!(s(doc, "state"), "new" | "updated" | "review_requested"))
        .count()
}

/// The collapsed row's docs marker: `[docs 3]`, or `[docs 3·1 new]` when
/// one is new, updated or awaiting review; empty without docs.
pub fn docs_marker(obligation: &Value) -> String {
    let total = array(obligation, "docs").len();
    match (total, unread_doc_count(obligation)) {
        (0, _) => String::new(),
        (total, 0) => format!("[docs {total}]"),
        (total, unread) => format!("[docs {total}·{unread} new]"),
    }
}

/// The claim that leads the collapsed row, and how many more there are:
/// the earliest ticket, or the first PR when no ticket is held.
pub fn lead_claim(obligation: &Value) -> Option<(Value, usize)> {
    let claims = array(obligation, "claims");
    let lead = claims
        .iter()
        .find(|claim| s(claim, "kind") == "ticket")
        .or_else(|| claims.first())?
        .clone();
    Some((lead, claims.len() - 1))
}

/// Sessions kept by `sm watch --repo/--role` and the typed search. A repo
/// filter alone also keeps the matches' ancestors and descendants, so the
/// tree around them stays intact.
pub fn filter_sessions(
    sessions: &[Value],
    repo_filter: Option<&str>,
    role_filter: Option<&str>,
    query: &str,
) -> Vec<Value> {
    let by_id: BTreeMap<_, _> = sessions.iter().map(|v| (s(v, "id"), v)).collect();
    let mut ids: BTreeSet<String> = sessions
        .iter()
        .filter(|v| {
            repo_filter.is_none_or(|r| {
                repo(v) == r || repo(v).starts_with(&format!("{}/", r.trim_end_matches('/')))
            }) && role_filter.is_none_or(|r| s(v, "role").eq_ignore_ascii_case(r))
                && (query.is_empty()
                    || format!(
                        "{} {}",
                        v,
                        by_id
                            .get(s(v, "parent_session_id"))
                            .map(|p| name(p))
                            .unwrap_or("")
                    )
                    .to_lowercase()
                    .contains(&query.to_lowercase()))
        })
        .map(|v| s(v, "id").to_owned())
        .collect();
    if repo_filter.is_some() && role_filter.is_none() && query.is_empty() {
        let matched = ids.clone();
        for id in &matched {
            let mut parent = by_id
                .get(id.as_str())
                .map(|v| s(v, "parent_session_id"))
                .unwrap_or("");
            let mut seen = BTreeSet::new();
            while let Some(v) = by_id.get(parent) {
                if !seen.insert(parent) {
                    break;
                }
                ids.insert(parent.into());
                parent = s(v, "parent_session_id");
            }
        }
        let mut descendants = matched;
        loop {
            let before = descendants.len();
            for v in sessions {
                if descendants.contains(s(v, "parent_session_id")) {
                    descendants.insert(s(v, "id").into());
                }
            }
            if before == descendants.len() {
                break;
            }
        }
        ids.extend(descendants);
    }
    sessions
        .iter()
        .filter(|v| ids.contains(s(v, "id")))
        .cloned()
        .collect()
}

/// One session's place in the default view's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Index into the slice given to [`tree_order`].
    pub index: usize,
    pub depth: usize,
    /// For a top-level entry that starts a new repo group, that repo.
    pub group: Option<String>,
}

/// The default (live, name-sorted) view's order: repos alphabetically, the
/// top-level sessions of each by name, each followed depth-first by its
/// children by name. Sessions caught in a parent cycle come last at depth
/// 0, so none disappear.
pub fn tree_order(sessions: &[Value]) -> Vec<TreeEntry> {
    let mut sorted: Vec<usize> = (0..sessions.len()).collect();
    sorted.sort_by(|&a, &b| {
        name(&sessions[a])
            .to_lowercase()
            .cmp(&name(&sessions[b]).to_lowercase())
    });
    let is_listed = |id: &str| sessions.iter().any(|p| s(p, "id") == id);
    let repos: BTreeSet<&str> = sessions.iter().map(repo).collect();
    let mut entries = Vec::new();
    let mut visited = BTreeSet::new();
    for group in repos {
        let roots: Vec<usize> = sorted
            .iter()
            .copied()
            .filter(|&i| {
                repo(&sessions[i]) == group && !is_listed(s(&sessions[i], "parent_session_id"))
            })
            .collect();
        let mut first = true;
        for root in roots {
            let start = entries.len();
            walk(sessions, &sorted, root, 0, &mut visited, &mut entries);
            if first && entries.len() > start {
                entries[start].group = Some(group.to_owned());
                first = false;
            }
        }
    }
    for &i in &sorted {
        if !visited.contains(s(&sessions[i], "id")) {
            walk(sessions, &sorted, i, 0, &mut visited, &mut entries);
        }
    }
    entries
}

fn walk(
    sessions: &[Value],
    sorted: &[usize],
    index: usize,
    depth: usize,
    visited: &mut BTreeSet<String>,
    entries: &mut Vec<TreeEntry>,
) {
    let id = s(&sessions[index], "id");
    if !visited.insert(id.to_owned()) {
        return;
    }
    entries.push(TreeEntry {
        index,
        depth,
        group: None,
    });
    for &child in sorted {
        if s(&sessions[child], "parent_session_id") == id {
            walk(sessions, sorted, child, depth + 1, visited, entries);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session(id: &str, parent: &str, repo: &str) -> Value {
        json!({"id": id, "parent_session_id": parent, "working_dir": repo, "friendly_name": id})
    }

    #[test]
    fn tree_groups_repos_sorts_by_name_and_keeps_cycles() {
        let sessions = vec![
            session("zeta", "", "/b"),
            session("child-b", "alpha", "/a"),
            session("alpha", "", "/a"),
            session("child-a", "alpha", "/other"),
            session("grand", "child-b", "/a"),
            session("loop1", "loop2", "/c"),
            session("loop2", "loop1", "/c"),
        ];
        let entries = tree_order(&sessions);
        let order: Vec<(&str, usize, Option<&str>)> = entries
            .iter()
            .map(|e| (s(&sessions[e.index], "id"), e.depth, e.group.as_deref()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("alpha", 0, Some("/a")),
                ("child-a", 1, None),
                ("child-b", 1, None),
                ("grand", 2, None),
                ("zeta", 0, Some("/b")),
                ("loop1", 0, None),
                ("loop2", 1, None),
            ]
        );
    }

    #[test]
    fn docs_marker_counts_all_docs_and_the_unread_ones() {
        let obligation = |states: &[&str]| json!({"docs": states.iter().map(|state| json!({"state": state})).collect::<Vec<_>>()});
        assert_eq!(docs_marker(&obligation(&[])), "");
        assert_eq!(docs_marker(&json!({})), "");
        assert_eq!(docs_marker(&obligation(&["read", "reviewed"])), "[docs 2]");
        assert_eq!(
            docs_marker(&obligation(&["new", "read", "review_requested"])),
            "[docs 3·2 new]"
        );
    }

    #[test]
    fn waiting_is_an_idle_session_with_something_pending() {
        let idle = json!({"activity_state": "idle"});
        let working = json!({"activity_state": "working"});
        let pending = json!({"waiting_on": [{"kind": "review"}]});
        let nothing = json!({"waiting_on": []});
        assert_eq!(display_state(&idle, Some(&pending)), "waiting");
        assert_eq!(display_state(&idle, Some(&nothing)), "idle");
        assert_eq!(display_state(&idle, None), "idle");
        assert_eq!(display_state(&working, Some(&pending)), "working");
    }
}
