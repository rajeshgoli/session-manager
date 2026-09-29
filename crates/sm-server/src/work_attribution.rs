//! Which thread an agent's turn or second belongs to (sm#1662, ticket
//! #1675), shared by the Spend and Time analytics. A thread is a ticket plus
//! the PRs linked to it; a PR that links to no ticket is its own thread.
//!
//! The first rule that applies wins: the agent's latest active claim, its
//! parent's (up to five levels), a ticket number in its name, else "No
//! ticket" under the repo of its working folder. See
//! `docs/working/1662_analytics_redesign.html`, appendix C.

use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    process::Command,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::work_claims::canonical_repo;
use crate::work_history::parse_time;

/// How many ancestors the parent rule walks.
const MAX_PARENT_LEVELS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Basis {
    Claim,
    Parent,
    Name,
    None,
}

impl Basis {
    pub fn key(self) -> &'static str {
        match self {
            Basis::Claim => "claim",
            Basis::Parent => "parent",
            Basis::Name => "name",
            Basis::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Attribution {
    /// `owner/name`, lower-cased; a plain folder name when the working
    /// folder has no GitHub remote.
    pub repo: String,
    /// `None` is the repo's "No ticket" thread.
    pub thread: Option<i64>,
    pub basis: Basis,
}

#[derive(Debug, Clone)]
struct Claim {
    id: String,
    repo: String,
    number: i64,
    is_pr: bool,
    claimed_at: i128,
    ended_at: Option<i128>,
}

impl Claim {
    fn active_at(&self, at: i128) -> bool {
        self.claimed_at <= at && self.ended_at.is_none_or(|ended| at < ended)
    }
}

/// A tracked ticket or PR's cached title and state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemInfo {
    pub is_pr: bool,
    pub title: String,
    pub state: String,
}

/// Claims, PR links and items from the queue DB, plus agent names and
/// parents from the usage DB.
#[derive(Debug, Clone, Default)]
pub struct Attributor {
    claims_by_seat: HashMap<String, Vec<Claim>>,
    /// `(repo, pr)` → linked tickets, either link source.
    pr_tickets: HashMap<(String, i64), BTreeSet<i64>>,
    items: HashMap<(String, i64), ItemInfo>,
    parents: HashMap<String, String>,
    names: HashMap<String, String>,
}

impl Attributor {
    /// Reads `work_claims`, `work_links` and `work_items` from the queue DB
    /// (a missing DB or table reads empty) and `seat_meta` from `usage`.
    pub fn load(queue_db: &Path, usage: &Connection) -> Result<Self> {
        let mut attributor = Self::default();
        attributor.load_seats(usage)?;
        if !queue_db.exists() {
            return Ok(attributor);
        }
        let conn = Connection::open_with_flags(queue_db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        if table_exists(&conn, "work_claims")? {
            let mut statement = conn.prepare(
                "SELECT id, repo, number, kind, session_id, claimed_at, ended_at
                   FROM work_claims WHERE reserved_at IS NULL",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?;
            for row in rows {
                let (id, repo, number, kind, session_id, claimed_at, ended_at) = row?;
                attributor.add_claim(
                    &session_id,
                    &id,
                    &repo,
                    number,
                    kind == "pr",
                    &claimed_at,
                    ended_at.as_deref(),
                );
            }
        }
        if table_exists(&conn, "work_links")? {
            let mut statement =
                conn.prepare("SELECT repo, pr_number, ticket_number FROM work_links")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (repo, pr, ticket) = row?;
                attributor.add_link(&repo, pr, ticket);
            }
        }
        if table_exists(&conn, "work_items")? {
            let mut statement =
                conn.prepare("SELECT repo, number, kind, title, state FROM work_items")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            for row in rows {
                let (repo, number, kind, title, state) = row?;
                attributor.add_item(&repo, number, kind == "pr", &title, &state);
            }
        }
        Ok(attributor)
    }

    /// Latest non-empty name and latest parent per seat.
    fn load_seats(&mut self, usage: &Connection) -> Result<()> {
        if !table_exists(usage, "seat_meta")? {
            return Ok(());
        }
        let mut statement = usage.prepare(
            "SELECT seat_id, friendly_name, parent_seat_id FROM seat_meta
              ORDER BY seat_id, observed_at DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut parent_seen = BTreeSet::new();
        for row in rows {
            let (seat, name, parent) = row?;
            if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
                self.names.entry(seat.clone()).or_insert(name);
            }
            if parent_seen.insert(seat.clone()) {
                if let Some(parent) = parent.filter(|parent| !parent.is_empty()) {
                    self.parents.insert(seat, parent);
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_claim(
        &mut self,
        seat: &str,
        id: &str,
        repo: &str,
        number: i64,
        is_pr: bool,
        claimed_at: &str,
        ended_at: Option<&str>,
    ) {
        let Some(claimed_at) = nanos(claimed_at) else {
            return;
        };
        let ended_at = match ended_at {
            Some(ended) => match nanos(ended) {
                Some(ended) => Some(ended),
                None => return,
            },
            None => None,
        };
        self.claims_by_seat
            .entry(seat.to_owned())
            .or_default()
            .push(Claim {
                id: id.to_owned(),
                repo: canonical_repo(repo),
                number,
                is_pr,
                claimed_at,
                ended_at,
            });
    }

    pub fn add_link(&mut self, repo: &str, pr: i64, ticket: i64) {
        self.pr_tickets
            .entry((canonical_repo(repo), pr))
            .or_default()
            .insert(ticket);
    }

    pub fn add_item(&mut self, repo: &str, number: i64, is_pr: bool, title: &str, state: &str) {
        self.items.insert(
            (canonical_repo(repo), number),
            ItemInfo {
                is_pr,
                title: title.to_owned(),
                state: state.to_owned(),
            },
        );
    }

    pub fn set_seat(&mut self, seat: &str, name: Option<&str>, parent: Option<&str>) {
        if let Some(name) = name {
            self.names.insert(seat.to_owned(), name.to_owned());
        }
        if let Some(parent) = parent {
            self.parents.insert(seat.to_owned(), parent.to_owned());
        }
    }

    pub fn name(&self, seat: &str) -> Option<&str> {
        self.names.get(seat).map(String::as_str)
    }

    pub fn item(&self, repo: &str, number: i64) -> Option<&ItemInfo> {
        self.items.get(&(canonical_repo(repo), number))
    }

    pub fn parent(&self, seat: &str) -> Option<&str> {
        self.parents.get(seat).map(String::as_str)
    }

    /// Whether `seat` itself holds an active claim at `at`.
    pub fn holds_claim_at(&self, seat: &str, at: i128) -> bool {
        self.claims_by_seat
            .get(seat)
            .is_some_and(|claims| claims.iter().any(|claim| claim.active_at(at)))
    }

    /// Every instant at which `attribute(seat, …)` or `holds_claim_at` can
    /// change: claim starts and ends of the seat and of the ancestors the
    /// parent rule walks.
    pub fn claim_boundaries(&self, seat: &str) -> Vec<i128> {
        let mut boundaries = Vec::new();
        let mut current = seat;
        let mut visited = BTreeSet::from([seat]);
        for level in 0..=MAX_PARENT_LEVELS {
            for claim in self.claims_by_seat.get(current).into_iter().flatten() {
                boundaries.push(claim.claimed_at);
                boundaries.extend(claim.ended_at);
            }
            if level == MAX_PARENT_LEVELS {
                break;
            }
            match self.parents.get(current) {
                Some(parent) if visited.insert(parent.as_str()) => current = parent,
                _ => break,
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        boundaries
    }

    /// The thread `seat`'s work at `at` (Unix nanoseconds) belongs to.
    /// `folder_repo` is the repo of the seat's working folder.
    pub fn attribute(&self, seat: &str, at: i128, folder_repo: &str) -> Attribution {
        if let Some((repo, thread)) = self.claim_thread(seat, at) {
            return Attribution {
                repo,
                thread: Some(thread),
                basis: Basis::Claim,
            };
        }
        let mut ancestor = seat;
        let mut visited = BTreeSet::from([seat]);
        for _ in 0..MAX_PARENT_LEVELS {
            let Some(parent) = self.parents.get(ancestor) else {
                break;
            };
            if !visited.insert(parent.as_str()) {
                break;
            }
            if let Some((repo, thread)) = self.claim_thread(parent, at) {
                return Attribution {
                    repo,
                    thread: Some(thread),
                    basis: Basis::Parent,
                };
            }
            ancestor = parent;
        }
        let folder_repo = canonical_repo(folder_repo);
        if let Some(number) = self.names.get(seat).and_then(|name| name_number(name)) {
            if let Some(item) = self.items.get(&(folder_repo.clone(), number)) {
                let thread = if item.is_pr {
                    self.pr_thread(&folder_repo, number)
                } else {
                    number
                };
                return Attribution {
                    repo: folder_repo,
                    thread: Some(thread),
                    basis: Basis::Name,
                };
            }
        }
        Attribution {
            repo: folder_repo,
            thread: None,
            basis: Basis::None,
        }
    }

    /// Rule 1 for one seat: its most recently taken active claim, mapped to
    /// a thread.
    fn claim_thread(&self, seat: &str, at: i128) -> Option<(String, i64)> {
        let claims = self.claims_by_seat.get(seat)?;
        let latest = claims
            .iter()
            .filter(|claim| claim.active_at(at))
            .max_by(|left, right| {
                left.claimed_at
                    .cmp(&right.claimed_at)
                    .then_with(|| left.id.cmp(&right.id))
            })?;
        if !latest.is_pr {
            return Some((latest.repo.clone(), latest.number));
        }
        let Some(tickets) = self.pr_tickets.get(&(latest.repo.clone(), latest.number)) else {
            return Some((latest.repo.clone(), latest.number));
        };
        let held = tickets.iter().copied().find(|ticket| {
            claims.iter().any(|claim| {
                !claim.is_pr
                    && claim.repo == latest.repo
                    && claim.number == *ticket
                    && claim.active_at(at)
            })
        });
        let thread = held.or_else(|| tickets.first().copied())?;
        Some((latest.repo.clone(), thread))
    }

    /// A PR's thread with no claim to consult: its lowest linked ticket, or
    /// the PR itself when it links to none.
    pub fn pr_thread(&self, repo: &str, pr: i64) -> i64 {
        self.pr_tickets
            .get(&(canonical_repo(repo), pr))
            .and_then(|tickets| tickets.first().copied())
            .unwrap_or(pr)
    }
}

/// The first `-`-delimited run of 3–5 digits in an agent name:
/// `1787-spec-reviewer` → 1787, `sm-1662-spec-author` → 1662.
pub fn name_number(name: &str) -> Option<i64> {
    name.split('-')
        .find(|part| (3..=5).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|part| part.parse().ok())
}

/// The repo of a ledger `project_key` (a git common dir or a plain folder):
/// its GitHub origin as `owner/name`, else the folder's name. Cached for
/// the process lifetime.
pub fn folder_repo(project_key: &str) -> String {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(repo) = cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(project_key).cloned())
    {
        return repo;
    }
    let repo = git_origin_github_repo(project_key)
        .map(|repo| canonical_repo(&repo))
        .unwrap_or_else(|| folder_name(project_key));
    if let Ok(mut cache) = cache.lock() {
        cache.insert(project_key.to_owned(), repo.clone());
    }
    repo
}

/// `/x/qwen-image/.git` and `/x/qwen-image` → `qwen-image`.
pub fn folder_name(project_key: &str) -> String {
    let trimmed = project_key.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/.git").unwrap_or(trimmed);
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    canonical_repo(last.strip_suffix(".git").unwrap_or(last))
}

pub fn git_origin_github_repo(dir: &str) -> Option<String> {
    let mut command = Command::new("git");
    command.args(["-C", dir, "remote", "get-url", "origin"]);
    let output = crate::child_output::output_with_timeout(command, Duration::from_secs(5)).ok()?;
    if !output.status.success() {
        return None;
    }
    github_repo_from_remote_url(String::from_utf8_lossy(&output.stdout).trim())
}

/// `git@github.com:owner/name.git`, `ssh://git@github.com/owner/name`, and
/// `https://github.com/owner/name.git` all yield `owner/name`.
pub fn github_repo_from_remote_url(url: &str) -> Option<String> {
    let path = [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
    ]
    .iter()
    .find_map(|prefix| url.strip_prefix(prefix))?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| format!("{owner}/{name}"))
}

pub fn nanos(value: &str) -> Option<i128> {
    parse_time(value).map(|time| time.unix_timestamp_nanos())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "rajeshgoli/fractal-algo-rust";

    fn at(value: &str) -> i128 {
        nanos(value).unwrap()
    }

    fn attribution(repo: &str, thread: Option<i64>, basis: Basis) -> Attribution {
        Attribution {
            repo: repo.to_owned(),
            thread,
            basis,
        }
    }

    /// Figure 3: a Codex engineer holding four claims on Sunday night.
    fn figure_three() -> Attributor {
        let mut attributor = Attributor::default();
        attributor.add_claim("eng", "c1", REPO, 1805, false, "2026-09-27T23:35:00Z", None);
        attributor.add_claim("eng", "c2", REPO, 1747, false, "2026-09-27T23:40:00Z", None);
        attributor.add_claim(
            "eng",
            "c3",
            REPO,
            1827,
            false,
            "2026-09-27T23:54:00Z",
            Some("2026-09-28T00:09:00Z"),
        );
        attributor.add_claim(
            "eng",
            "c4",
            "RajeshGoli/Fractal-Algo-Rust",
            1828,
            true,
            "2026-09-27T23:56:00Z",
            Some("2026-09-28T00:24:00Z"),
        );
        attributor.add_link(REPO, 1828, 1827);
        attributor.add_link(REPO, 1828, 1830);
        attributor
    }

    #[test]
    fn latest_active_claim_wins_and_falls_back_when_it_ends() {
        let attributor = figure_three();
        let cases = [
            ("2026-09-27T23:45:00Z", 1747),
            // PR #1828 is the latest claim; it links to #1827 (held) and #1830.
            ("2026-09-27T23:58:00Z", 1827),
            // #1827's claim ended; the PR still goes to its lowest link.
            ("2026-09-28T00:15:00Z", 1827),
            // PR merged: back to the older open claim.
            ("2026-09-28T00:30:00Z", 1747),
        ];
        for (minute, thread) in cases {
            assert_eq!(
                attributor.attribute("eng", at(minute), "anything"),
                attribution(REPO, Some(thread), Basis::Claim),
                "{minute}"
            );
        }
    }

    #[test]
    fn claims_taken_at_the_same_instant_tie_break_by_id() {
        let mut attributor = Attributor::default();
        attributor.add_claim("eng", "a", REPO, 10, false, "2026-09-27T10:00:00Z", None);
        attributor.add_claim("eng", "b", REPO, 20, false, "2026-09-27T10:00:00Z", None);
        assert_eq!(
            attributor
                .attribute("eng", at("2026-09-27T11:00:00Z"), REPO)
                .thread,
            Some(20)
        );
        // A claim covers [claimed_at, ended_at): not the instant it ends.
        attributor.add_claim(
            "eng",
            "z",
            REPO,
            30,
            false,
            "2026-09-27T10:30:00Z",
            Some("2026-09-27T11:00:00Z"),
        );
        assert_eq!(
            attributor
                .attribute("eng", at("2026-09-27T10:59:00Z"), REPO)
                .thread,
            Some(30)
        );
        assert_eq!(
            attributor
                .attribute("eng", at("2026-09-27T11:00:00Z"), REPO)
                .thread,
            Some(20)
        );
    }

    #[test]
    fn pr_claims_map_to_linked_tickets() {
        let mut attributor = Attributor::default();
        let t = "2026-09-27T12:00:00Z";
        // Linked to #5 and #3, the seat holds #5: #5.
        attributor.add_claim("a", "1", REPO, 5, false, "2026-09-27T10:00:00Z", None);
        attributor.add_claim("a", "2", REPO, 100, true, "2026-09-27T11:00:00Z", None);
        attributor.add_link(REPO, 100, 5);
        attributor.add_link(REPO, 100, 3);
        assert_eq!(attributor.attribute("a", at(t), REPO).thread, Some(5));
        // Same PR, a seat that holds neither ticket: the lowest, #3.
        attributor.add_claim("b", "3", REPO, 100, true, "2026-09-27T11:00:00Z", None);
        assert_eq!(attributor.attribute("b", at(t), REPO).thread, Some(3));
        // An unlinked PR is its own thread.
        attributor.add_claim("c", "4", REPO, 200, true, "2026-09-27T11:00:00Z", None);
        assert_eq!(attributor.attribute("c", at(t), REPO).thread, Some(200));
        assert_eq!(attributor.pr_thread(REPO, 100), 3);
        assert_eq!(attributor.pr_thread(REPO, 200), 200);
    }

    #[test]
    fn parent_rule_walks_up_to_five_levels() {
        let mut attributor = Attributor::default();
        attributor.add_claim("root", "1", REPO, 1662, false, "2026-09-27T10:00:00Z", None);
        let chain = ["l1", "l2", "l3", "l4", "l5", "l6"];
        attributor.set_seat("l1", None, Some("root"));
        for pair in chain.windows(2) {
            attributor.set_seat(pair[1], None, Some(pair[0]));
        }
        let t = at("2026-09-27T12:00:00Z");
        // A grandchild of the claiming seat.
        assert_eq!(
            attributor.attribute("l2", t, "rajeshgoli/other"),
            attribution(REPO, Some(1662), Basis::Parent)
        );
        // Five levels up reaches root; six does not.
        assert_eq!(attributor.attribute("l5", t, REPO).basis, Basis::Parent);
        assert_eq!(
            attributor.attribute("l6", t, REPO),
            attribution(REPO, None, Basis::None)
        );
        // A parent cycle ends the walk.
        attributor.set_seat("x", None, Some("y"));
        attributor.set_seat("y", None, Some("x"));
        assert_eq!(attributor.attribute("x", t, REPO).basis, Basis::None);
    }

    #[test]
    fn name_rule_applies_only_to_numbers_the_repo_tracks() {
        let mut attributor = Attributor::default();
        attributor.add_item(REPO, 1787, false, "Pivot detector", "open");
        attributor.add_item(REPO, 1850, true, "Fix", "merged");
        attributor.add_link(REPO, 1850, 1849);
        attributor.set_seat("known", Some("1787-spec-reviewer"), None);
        attributor.set_seat("unknown", Some("1788-spec-reviewer"), None);
        attributor.set_seat("pr", Some("sm-1850-reviewer-2"), None);
        let t = at("2026-09-27T12:00:00Z");
        assert_eq!(
            attributor.attribute("known", t, REPO),
            attribution(REPO, Some(1787), Basis::Name)
        );
        assert_eq!(
            attributor.attribute("unknown", t, REPO),
            attribution(REPO, None, Basis::None)
        );
        // The number must exist in the working folder's repo.
        assert_eq!(
            attributor.attribute("known", t, "rajeshgoli/session-manager"),
            attribution("rajeshgoli/session-manager", None, Basis::None)
        );
        // A PR number maps to its linked ticket.
        assert_eq!(attributor.attribute("pr", t, REPO).thread, Some(1849));
    }

    #[test]
    fn name_numbers_are_whole_dash_delimited_runs() {
        assert_eq!(name_number("1787-spec-reviewer"), Some(1787));
        assert_eq!(name_number("sm-1662-spec-author"), Some(1662));
        assert_eq!(name_number("sm-1637-engineer"), Some(1637));
        assert_eq!(name_number("pr-12-reviewer"), None);
        assert_eq!(name_number("claude-123456"), None);
        assert_eq!(name_number("iter7-plan"), None);
        assert_eq!(name_number("comfy-ui"), None);
    }

    #[test]
    fn folder_names_drop_the_git_dir() {
        assert_eq!(
            folder_name("/Users/r/Pictures/qwen-image/.git"),
            "qwen-image"
        );
        assert_eq!(folder_name("/Users/r/Pictures/qwen-image"), "qwen-image");
        assert_eq!(folder_name("/Users/r/bare/Repo.git"), "repo");
    }
}
