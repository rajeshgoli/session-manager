//! Reading and writing GitHub for the board (appendix C): the per-repo read
//! query and its follow-ups, the node-id lookup and the four link mutations.
//! The transport sits behind [`BoardSource`] so tests substitute a fake.

use std::collections::BTreeMap;

use serde_json::Value;

use super::model::{Key, PrRef};

/// Connections the read query caps at 50 nodes per issue.
pub const CONNECTION_PAGE: usize = 50;
/// A pass stops reading below this many GraphQL points (C4).
pub const RATE_LIMIT_FLOOR: i64 = 200;

/// An issue as another issue's `parent`, `blockedBy` or `subIssues` node
/// shows it, or as a lookup returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefNode {
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub url: String,
    /// `open` or `closed`.
    pub state: String,
    pub state_reason: Option<String>,
    pub closed_at: Option<String>,
}

impl RefNode {
    pub fn key(&self) -> Key {
        (self.repo.clone(), self.number)
    }
}

/// One open issue from the read query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueNode {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub updated_at: Option<String>,
    pub state_reason: Option<String>,
    pub parent: Option<RefNode>,
    pub blocked_by: Vec<RefNode>,
    /// Set when `blockedBy` has more nodes: the cursor after the first page.
    pub blocked_by_more: Option<String>,
    pub sub_issues: Vec<RefNode>,
    pub sub_issues_more: Option<String>,
    pub prs: Vec<PrRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssuesPage {
    pub rate_remaining: Option<i64>,
    pub rate_reset_at: Option<String>,
    pub nodes: Vec<IssueNode>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueConnection {
    BlockedBy,
    SubIssues,
}

impl IssueConnection {
    fn field(self) -> &'static str {
        match self {
            Self::BlockedBy => "blockedBy",
            Self::SubIssues => "subIssues",
        }
    }
}

/// A ticket as the link writer resolves it (C5 step 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIssue {
    pub id: String,
    pub node: RefNode,
    pub parent: Option<Key>,
    pub blocked_by: Vec<Key>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkMutation {
    AddBlockedBy {
        issue_id: String,
        blocking_id: String,
    },
    RemoveBlockedBy {
        issue_id: String,
        blocking_id: String,
    },
    AddSubIssue {
        parent_id: String,
        child_id: String,
    },
    RemoveSubIssue {
        parent_id: String,
        child_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// GitHub answered and refused: its message, verbatim.
    Refused(String),
    /// GitHub could not be reached.
    Transport(String),
}

/// Where the board's GitHub data comes from.
pub trait BoardSource: Send + Sync {
    /// One page of a repo's open issues.
    fn issues_page(&self, repo: &str, cursor: Option<&str>) -> Result<IssuesPage, String>;
    /// The rest of one issue's `blockedBy` or `subIssues`, after `cursor`:
    /// the nodes and the next cursor.
    fn connection_page(
        &self,
        repo: &str,
        number: i64,
        connection: IssueConnection,
        cursor: &str,
    ) -> Result<(Vec<RefNode>, Option<String>), String>;
    /// Issues by number (at most 50); `None` for one GitHub returns null.
    fn items(&self, repo: &str, numbers: &[i64]) -> Result<BTreeMap<i64, Option<RefNode>>, String>;
    /// Node ids and current links; `None` for an issue that doesn't exist.
    fn resolve(&self, issues: &[Key]) -> Result<Vec<Option<ResolvedIssue>>, String>;
    fn write_link(&self, mutation: &LinkMutation) -> Result<(), WriteError>;
}

fn graphql_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

fn split_repo(repo: &str) -> (&str, &str) {
    repo.split_once('/').unwrap_or((repo, ""))
}

const REF_FRAGMENT: &str = "fragment Ref on Issue { number title url state stateReason closedAt \
     repository { nameWithOwner } }";

/// The C2 read query for one page of `repo`'s open issues.
pub fn issues_query(repo: &str, cursor: Option<&str>) -> String {
    let (owner, name) = split_repo(repo);
    let after = cursor
        .map(|cursor| format!(", after: {}", graphql_string(cursor)))
        .unwrap_or_default();
    format!(
        "query {{
  rateLimit {{ remaining resetAt }}
  repository(owner: {}, name: {}) {{
    issues(states: OPEN, first: 100{after}) {{
      pageInfo {{ hasNextPage endCursor }}
      nodes {{
        number title url updatedAt stateReason
        parent {{ ...Ref }}
        blockedBy(first: {CONNECTION_PAGE}) {{ totalCount pageInfo {{ hasNextPage endCursor }} nodes {{ ...Ref }} }}
        subIssues(first: {CONNECTION_PAGE}) {{ totalCount pageInfo {{ hasNextPage endCursor }} nodes {{ ...Ref }} }}
        closedByPullRequestsReferences(first: 10, includeClosedPrs: true) {{
          nodes {{ number state url repository {{ nameWithOwner }} }} }}
      }}
    }}
  }}
}}
{REF_FRAGMENT}",
        graphql_string(owner),
        graphql_string(name),
    )
}

/// The follow-up for one issue whose connection has more than 50 nodes.
pub fn connection_query(
    repo: &str,
    number: i64,
    connection: IssueConnection,
    cursor: &str,
) -> String {
    let (owner, name) = split_repo(repo);
    format!(
        "query {{ repository(owner: {}, name: {}) {{ issue(number: {number}) {{
  {}(first: 100, after: {}) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ ...Ref }} }} }} }} }}
{REF_FRAGMENT}",
        graphql_string(owner),
        graphql_string(name),
        connection.field(),
        graphql_string(cursor),
    )
}

/// C5 step 1: node ids, parents and blockers for each ticket, one alias per
/// ticket.
pub fn resolve_query(issues: &[Key]) -> String {
    let aliases = issues
        .iter()
        .enumerate()
        .map(|(index, (repo, number))| {
            let (owner, name) = split_repo(repo);
            format!(
                "  r{index}: repository(owner: {}, name: {}) {{ issue(number: {number}) {{ id ...Ref \
                 parent {{ number repository {{ nameWithOwner }} }} \
                 blockedBy(first: 50) {{ nodes {{ number repository {{ nameWithOwner }} }} }} }} }}",
                graphql_string(owner),
                graphql_string(name),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("query {{\n{aliases}\n}}\n{REF_FRAGMENT}")
}

pub fn mutation_query(mutation: &LinkMutation) -> String {
    match mutation {
        LinkMutation::AddBlockedBy {
            issue_id,
            blocking_id,
        } => format!(
            "mutation {{ addBlockedBy(input: {{issueId: {}, blockingIssueId: {}}}) {{ issue {{ id }} }} }}",
            graphql_string(issue_id),
            graphql_string(blocking_id)
        ),
        LinkMutation::RemoveBlockedBy {
            issue_id,
            blocking_id,
        } => format!(
            "mutation {{ removeBlockedBy(input: {{issueId: {}, blockingIssueId: {}}}) {{ issue {{ id }} }} }}",
            graphql_string(issue_id),
            graphql_string(blocking_id)
        ),
        LinkMutation::AddSubIssue {
            parent_id,
            child_id,
        } => format!(
            "mutation {{ addSubIssue(input: {{issueId: {}, subIssueId: {}, replaceParent: false}}) {{ issue {{ id }} }} }}",
            graphql_string(parent_id),
            graphql_string(child_id)
        ),
        LinkMutation::RemoveSubIssue {
            parent_id,
            child_id,
        } => format!(
            "mutation {{ removeSubIssue(input: {{issueId: {}, subIssueId: {}}}) {{ issue {{ id }} }} }}",
            graphql_string(parent_id),
            graphql_string(child_id)
        ),
    }
}

fn parse_json(stdout: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(stdout).map_err(|error| format!("invalid GraphQL response: {error}"))
}

/// The first GraphQL error message, if any.
pub fn graphql_error(value: &Value) -> Option<String> {
    value["errors"].as_array().and_then(|errors| {
        errors
            .iter()
            .filter_map(|error| error["message"].as_str())
            .map(ToOwned::to_owned)
            .reduce(|a, b| format!("{a}; {b}"))
    })
}

fn opt_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn node_repo(node: &Value) -> Option<String> {
    node["repository"]["nameWithOwner"]
        .as_str()
        .map(crate::work_claims::canonical_repo)
}

pub fn parse_ref(node: &Value) -> Option<RefNode> {
    Some(RefNode {
        repo: node_repo(node)?,
        number: node["number"].as_i64()?,
        title: node["title"].as_str().unwrap_or_default().to_owned(),
        url: node["url"].as_str().unwrap_or_default().to_owned(),
        state: node["state"]
            .as_str()
            .unwrap_or("OPEN")
            .to_ascii_lowercase(),
        state_reason: opt_string(&node["stateReason"]),
        closed_at: opt_string(&node["closedAt"]),
    })
}

fn parse_refs(nodes: &Value) -> Vec<RefNode> {
    nodes
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_ref)
        .collect()
}

fn more_cursor(connection: &Value) -> Option<String> {
    let more = connection["pageInfo"]["hasNextPage"].as_bool() == Some(true)
        || connection["totalCount"].as_u64().unwrap_or(0) as usize
            > connection["nodes"].as_array().map_or(0, Vec::len);
    if !more {
        return None;
    }
    opt_string(&connection["pageInfo"]["endCursor"])
}

pub fn parse_issues_page(stdout: &[u8]) -> Result<IssuesPage, String> {
    let value = parse_json(stdout)?;
    let issues = &value["data"]["repository"]["issues"];
    if issues.is_null() {
        return Err(graphql_error(&value).unwrap_or_else(|| "no repository in response".into()));
    }
    let nodes = issues["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| {
            Some(IssueNode {
                number: node["number"].as_i64()?,
                title: node["title"].as_str().unwrap_or_default().to_owned(),
                url: node["url"].as_str().unwrap_or_default().to_owned(),
                updated_at: opt_string(&node["updatedAt"]),
                state_reason: opt_string(&node["stateReason"]),
                parent: parse_ref(&node["parent"]),
                blocked_by: parse_refs(&node["blockedBy"]["nodes"]),
                blocked_by_more: more_cursor(&node["blockedBy"]),
                sub_issues: parse_refs(&node["subIssues"]["nodes"]),
                sub_issues_more: more_cursor(&node["subIssues"]),
                prs: node["closedByPullRequestsReferences"]["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|pr| {
                        Some(PrRef {
                            repo: node_repo(pr)?,
                            number: pr["number"].as_i64()?,
                            state: pr["state"].as_str().unwrap_or("OPEN").to_owned(),
                            url: pr["url"].as_str().unwrap_or_default().to_owned(),
                        })
                    })
                    .collect(),
            })
        })
        .collect();
    let next_cursor = (issues["pageInfo"]["hasNextPage"].as_bool() == Some(true))
        .then(|| opt_string(&issues["pageInfo"]["endCursor"]))
        .flatten();
    Ok(IssuesPage {
        rate_remaining: value["data"]["rateLimit"]["remaining"].as_i64(),
        rate_reset_at: opt_string(&value["data"]["rateLimit"]["resetAt"]),
        nodes,
        next_cursor,
    })
}

pub fn parse_connection_page(
    stdout: &[u8],
    connection: IssueConnection,
) -> Result<(Vec<RefNode>, Option<String>), String> {
    let value = parse_json(stdout)?;
    let page = &value["data"]["repository"]["issue"][connection.field()];
    if page.is_null() {
        return Err(graphql_error(&value).unwrap_or_else(|| "no issue in response".into()));
    }
    let next = (page["pageInfo"]["hasNextPage"].as_bool() == Some(true))
        .then(|| opt_string(&page["pageInfo"]["endCursor"]))
        .flatten();
    Ok((parse_refs(&page["nodes"]), next))
}

pub fn parse_resolve(stdout: &[u8], count: usize) -> Result<Vec<Option<ResolvedIssue>>, String> {
    let value = parse_json(stdout)?;
    if value["data"].is_null() {
        return Err(graphql_error(&value).unwrap_or_else(|| "no data in response".into()));
    }
    let key_of = |node: &Value| Some((node_repo(node)?, node["number"].as_i64()?));
    Ok((0..count)
        .map(|index| {
            let issue = &value["data"][format!("r{index}")]["issue"];
            Some(ResolvedIssue {
                id: issue["id"].as_str()?.to_owned(),
                node: parse_ref(issue)?,
                parent: key_of(&issue["parent"]),
                blocked_by: issue["blockedBy"]["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(key_of)
                    .collect(),
            })
        })
        .collect())
}

/// `issueOrPullRequest` lookups (the claims `items_query` batch) as board
/// nodes; pull requests read as missing, since they are never tickets.
pub fn items_from_batch(
    repo: &str,
    numbers: &[i64],
    batch: &crate::work_claims::BatchFetch,
) -> BTreeMap<i64, Option<RefNode>> {
    use crate::work_claims::{ItemFetch, WorkKind};
    numbers
        .iter()
        .filter_map(|number| {
            let node = match batch.get(number)? {
                ItemFetch::Found(item) if item.kind == WorkKind::Ticket => Some(RefNode {
                    repo: crate::work_claims::canonical_repo(repo),
                    number: *number,
                    title: item.title.clone(),
                    url: item.url.clone(),
                    state: item.state.to_ascii_lowercase(),
                    state_reason: item.state_reason.clone(),
                    closed_at: item.closed_at.clone(),
                }),
                _ => None,
            };
            Some((*number, node))
        })
        .collect()
}

/// A repo's full read: every open issue with complete connections, and the
/// lowest rate-limit reading seen.
#[derive(Debug, Clone, Default)]
pub struct RepoRead {
    pub issues: Vec<IssueNode>,
    pub rate_remaining: Option<i64>,
    pub rate_reset_at: Option<String>,
    /// The read stopped early because the rate limit ran low.
    pub rate_limited: bool,
}

/// Pages through a repo's open issues and completes every connection over
/// 50 nodes.
pub fn read_repo(source: &dyn BoardSource, repo: &str) -> Result<RepoRead, String> {
    let mut read = RepoRead::default();
    let mut cursor: Option<String> = None;
    loop {
        let page = source.issues_page(repo, cursor.as_deref())?;
        if let Some(remaining) = page.rate_remaining {
            if read.rate_remaining.is_none_or(|low| remaining < low) {
                read.rate_remaining = Some(remaining);
                read.rate_reset_at = page.rate_reset_at.clone();
            }
            if remaining < RATE_LIMIT_FLOOR {
                read.rate_limited = true;
                return Ok(read);
            }
        }
        read.issues.extend(page.nodes);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    for issue in &mut read.issues {
        for (connection, more, nodes) in [
            (
                IssueConnection::BlockedBy,
                issue.blocked_by_more.take(),
                &mut issue.blocked_by,
            ),
            (
                IssueConnection::SubIssues,
                issue.sub_issues_more.take(),
                &mut issue.sub_issues,
            ),
        ] {
            let mut cursor = more;
            while let Some(after) = cursor {
                let (page, next) =
                    source.connection_page(repo, issue.number, connection, &after)?;
                nodes.extend(page);
                cursor = next;
            }
        }
    }
    Ok(read)
}
