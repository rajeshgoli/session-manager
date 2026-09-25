//! `sm ticket <N> --setup-worktree` and `sm worktree keep` (sm#1452,
//! ticket #1487): sm creates the worktree a ticket is worked in, records it
//! on the claim, and deletes it at retire unless it is kept.

use super::*;

#[derive(Args)]
pub(crate) struct WorktreeArgs {
    #[command(subcommand)]
    command: WorktreeCommand,
}

#[derive(Subcommand)]
enum WorktreeCommand {
    /// Don't delete this worktree when you are retired
    Keep(KeepArgs),
}

#[derive(Args)]
struct KeepArgs {
    /// The worktree; defaults to the cwd's checkout
    #[arg(long)]
    path: Option<String>,
    /// Why it must outlive you (a running server, results not yet pushed)
    #[arg(long, required_unless_present = "off", conflicts_with = "off")]
    reason: Option<String>,
    /// Stop keeping it
    #[arg(long)]
    off: bool,
}

pub(crate) fn run_worktree(client: &ApiClient, args: WorktreeArgs) -> Result<()> {
    match args.command {
        WorktreeCommand::Keep(args) => run_keep(client, args),
    }
}

fn run_keep(client: &ApiClient, args: KeepArgs) -> Result<()> {
    let session_id = managed_session_id("sm worktree keep")?;
    let path = match args
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(path) => absolute_path(Path::new(path))?,
        None => {
            let cwd = env::current_dir()?;
            run_ok(
                &ProcessTools,
                "git",
                &cwd,
                &["rev-parse", "--show-toplevel"],
            )
            .map_err(|_| anyhow!("Not inside a git checkout; pass --path."))?
        }
    };
    let response = client.request(
        "POST",
        "/worktrees/keep",
        Some(json!({
            "requester_session_id": session_id,
            "path": path,
            "reason": args.reason,
            "off": args.off,
        })),
    )?;
    let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    finish(keep_output(response.status, &body))
}

fn absolute_path(path: &Path) -> Result<String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    Ok(fs::canonicalize(&path)
        .unwrap_or(path)
        .display()
        .to_string())
}

/// `Keeping ~/worktrees/x: <reason>.` / `No longer keeping ~/worktrees/x.`
pub(crate) fn keep_output(status: u16, body: &Value) -> Printed {
    if status != 200 {
        return Printed {
            stdout: Vec::new(),
            stderr: vec![api_detail(status, body)],
            exit: 1,
        };
    }
    let path = home_relative(body["path"].as_str().unwrap_or_default());
    let line = if body["kept"].as_bool() == Some(true) {
        format!(
            "Keeping {path}: {}.",
            body["reason"]
                .as_str()
                .unwrap_or_default()
                .trim_end_matches('.')
        )
    } else {
        format!("No longer keeping {path}.")
    };
    Printed {
        stdout: vec![line],
        stderr: Vec::new(),
        exit: 0,
    }
}

/// `sm retire` output: one line per worktree in the response.
pub(crate) fn retire_worktree_lines(payload: &Value) -> Vec<String> {
    payload["worktrees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|worktree| {
            let path = home_relative(worktree["path"].as_str()?);
            let reason = worktree["reason"].as_str().unwrap_or_default();
            Some(if worktree["removed"].as_bool() == Some(true) {
                format!("Deleted worktree {path} ({reason}).")
            } else if reason == "absent" {
                return None;
            } else {
                format!("Kept worktree {path}: {reason}.")
            })
        })
        .collect()
}

/// A ticket title (or a given slug) as a branch and directory slug: runs of
/// non-alphanumerics become `-`.
fn slugify(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// The title's first 4 words, cut to 30 characters and trimmed of `-`.
pub(crate) fn title_slug(title: &str) -> String {
    let joined = slugify(title)
        .into_iter()
        .take(4)
        .collect::<Vec<_>>()
        .join("-");
    joined
        .chars()
        .take(30)
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

/// What `--setup-worktree` creates: `<root>/<prefix>-<N>-<slug>` on branch
/// `<N>-<slug>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreePlan {
    pub path: PathBuf,
    pub branch: String,
}

pub(crate) fn plan_worktree(
    number: i64,
    title: &str,
    slug: Option<&str>,
    root: &Path,
    prefix: &str,
) -> Result<WorktreePlan> {
    let slug = match slug.map(str::trim).filter(|slug| !slug.is_empty()) {
        Some(given) => slugify(given).join("-"),
        None => title_slug(title),
    };
    if slug.is_empty() {
        bail!("Ticket #{number}'s title gives no slug; pass --setup-worktree=<slug>.");
    }
    Ok(WorktreePlan {
        path: root.join(format!("{prefix}-{number}-{slug}")),
        branch: format!("{number}-{slug}"),
    })
}

/// The claim a `POST /claims` returned, as setup needs it.
#[derive(Debug, Clone)]
pub(crate) struct SetupClaim {
    pub claim_id: String,
    pub number: i64,
    pub title: String,
    /// The managed worktree already recorded on the claim.
    pub recorded: Option<WorktreePlan>,
    pub root: PathBuf,
    pub prefix: String,
}

impl SetupClaim {
    pub(crate) fn from_response(body: &Value) -> Result<Self> {
        let claim = &body["claim"];
        let text = |key: &str| claim[key].as_str().unwrap_or_default().to_owned();
        let recorded = (claim["managed_worktree"].as_bool() == Some(true))
            .then(|| {
                Some(WorktreePlan {
                    path: PathBuf::from(claim["worktree_path"].as_str()?),
                    branch: claim["branch"].as_str()?.to_owned(),
                })
            })
            .flatten();
        let naming = &body["worktree_naming"];
        let root = naming["root"]
            .as_str()
            .filter(|root| !root.is_empty())
            .context("the server did not say where worktrees go (worktree_naming)")?;
        Ok(Self {
            claim_id: text("id"),
            number: claim["number"].as_i64().unwrap_or_default(),
            title: text("title"),
            recorded,
            root: PathBuf::from(root),
            prefix: naming["prefix"].as_str().unwrap_or_default().to_owned(),
        })
    }
}

/// How setup ended: the line it prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetupOutcome {
    Created {
        plan: WorktreePlan,
        base: String,
        base_sha: String,
    },
    Existing {
        plan: WorktreePlan,
    },
}

impl SetupOutcome {
    pub(crate) fn line(&self) -> String {
        match self {
            Self::Created {
                plan,
                base,
                base_sha,
            } => format!(
                "Worktree {} on branch {} from {base} ({}).",
                home_relative(&plan.path.display().to_string()),
                plan.branch,
                &base_sha[..base_sha.len().min(7)]
            ),
            Self::Existing { plan } => format!(
                "Worktree {} on branch {} (already set up).",
                home_relative(&plan.path.display().to_string()),
                plan.branch
            ),
        }
    }
}

/// `POST /claims/worktree` with `intent` or `created`.
pub(crate) trait WorktreeRecorder {
    fn record(&mut self, state: &str, plan: &WorktreePlan, base_sha: &str) -> Result<()>;
}

/// The git half of `--setup-worktree`, after the claim succeeded (appendix
/// G): names, base, reuse, intent, create, confirm. `repo_root` is the cwd's
/// checkout of the ticket's repo.
pub(crate) fn setup_worktree(
    tools: &dyn DocTools,
    repo_root: &Path,
    claim: &SetupClaim,
    slug: Option<&str>,
    base_flag: Option<&str>,
    recorder: &mut dyn WorktreeRecorder,
) -> Result<SetupOutcome> {
    let root = {
        fs::create_dir_all(&claim.root)
            .with_context(|| format!("failed to create {}", claim.root.display()))?;
        fs::canonicalize(&claim.root).unwrap_or_else(|_| claim.root.clone())
    };
    let plan = match &claim.recorded {
        Some(recorded) => {
            // A title change never makes a second worktree.
            let asked = slug
                .filter(|slug| !slug.trim().is_empty())
                .map(|slug| {
                    plan_worktree(claim.number, &claim.title, Some(slug), &root, &claim.prefix)
                })
                .transpose()?;
            if asked.is_some_and(|asked| asked.path != recorded.path) {
                bail!(
                    "This claim already has worktree {}.",
                    home_relative(&recorded.path.display().to_string())
                );
            }
            recorded.clone()
        }
        None => plan_worktree(claim.number, &claim.title, slug, &root, &claim.prefix)?,
    };
    let fetch = tools.run("git", repo_root, &["fetch", "origin"])?;
    if !fetch.success {
        bail!("git fetch origin failed: {}", fetch.stderr);
    }
    let base = match base_flag.map(str::trim).filter(|base| !base.is_empty()) {
        Some(base) => base.to_owned(),
        None => run_ok(
            tools,
            "git",
            repo_root,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
        .ok()
        .filter(|base| !base.is_empty())
        .unwrap_or_else(|| "origin/main".to_owned()),
    };
    let base_sha = run_ok(
        tools,
        "git",
        repo_root,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )
    .map_err(|_| anyhow!("Base {base} is not a commit in this repo."))?;
    let path_text = plan.path.display().to_string();
    let existing = if plan.path.exists() {
        if !is_worktree_on(tools, repo_root, &plan)? {
            bail!(
                "{} exists and is not this ticket's worktree.",
                home_relative(&path_text)
            );
        }
        true
    } else {
        false
    };
    recorder
        .record("intent", &plan, &base_sha)
        .context("could not record the worktree on the claim; nothing was created")?;
    if !existing {
        let branch_ref = format!("refs/heads/{}", plan.branch);
        let has_branch = tools
            .run(
                "git",
                repo_root,
                &["show-ref", "--verify", "--quiet", &branch_ref],
            )?
            .success;
        let add = if has_branch {
            tools.run(
                "git",
                repo_root,
                &["worktree", "add", &path_text, &plan.branch],
            )?
        } else {
            tools.run(
                "git",
                repo_root,
                &["worktree", "add", "-b", &plan.branch, &path_text, &base],
            )?
        };
        if !add.success {
            bail!("{}", add.stderr);
        }
    }
    // The intent already names the path, so a failed confirmation leaks
    // nothing: retire finds it either way.
    if let Err(error) = recorder.record("created", &plan, &base_sha) {
        eprintln!("Warning: could not confirm the worktree on the claim: {error:#}");
    }
    Ok(if existing {
        SetupOutcome::Existing { plan }
    } else {
        SetupOutcome::Created {
            plan,
            base,
            base_sha,
        }
    })
}

/// Whether `plan.path` is already a worktree of this repo on `plan.branch`.
fn is_worktree_on(tools: &dyn DocTools, repo_root: &Path, plan: &WorktreePlan) -> Result<bool> {
    let listing = run_ok(
        tools,
        "git",
        repo_root,
        &["worktree", "list", "--porcelain"],
    )?;
    let want = fs::canonicalize(&plan.path).unwrap_or_else(|_| plan.path.clone());
    let want_branch = format!("refs/heads/{}", plan.branch);
    let mut path = None;
    for line in listing.lines() {
        if let Some(worktree) = line.strip_prefix("worktree ") {
            path = Some(fs::canonicalize(worktree).unwrap_or_else(|_| PathBuf::from(worktree)));
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if path.as_ref() == Some(&want) && branch == want_branch {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

struct ApiRecorder<'a> {
    client: &'a ApiClient,
    session_id: &'a str,
    claim_id: &'a str,
}

impl WorktreeRecorder for ApiRecorder<'_> {
    fn record(&mut self, state: &str, plan: &WorktreePlan, base_sha: &str) -> Result<()> {
        let response = self.client.request(
            "POST",
            "/claims/worktree",
            Some(json!({
                "requester_session_id": self.session_id,
                "claim_id": self.claim_id,
                "state": state,
                "worktree_path": plan.path.display().to_string(),
                "branch": plan.branch,
                "base_sha": base_sha,
            })),
        )?;
        if response.status != 200 {
            let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
            bail!("{}", api_detail(response.status, &body));
        }
        Ok(())
    }
}

/// `sm ticket <N> --setup-worktree[=SLUG] [--base REF]`: claim first; a
/// refused claim creates nothing.
pub(crate) fn run_setup(
    client: &ApiClient,
    session_id: &str,
    number: i64,
    target: &ClaimTarget,
    take: bool,
    slug: &str,
    base: Option<&str>,
) -> Result<()> {
    let Some(repo_root) = target.worktree_path.as_deref() else {
        bail!(
            "--setup-worktree must run inside a checkout of {}",
            target.repo
        );
    };
    // The worktree is recorded by the intent, not from the cwd.
    let claim_target = ClaimTarget {
        worktree_path: None,
        branch: None,
        ..target.clone()
    };
    let response = client.request(
        "POST",
        "/claims",
        Some(claim_body(
            session_id,
            "ticket",
            number,
            &claim_target,
            take,
            &[],
        )),
    )?;
    let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    let printed = claim_output("ticket", number, response.status, &body, |path| {
        client.url_for(path)
    });
    if printed.exit != 0 {
        return finish(printed);
    }
    finish(printed)?;
    let claim = SetupClaim::from_response(&body)?;
    let mut recorder = ApiRecorder {
        client,
        session_id,
        claim_id: &claim.claim_id,
    };
    let slug = (!slug.trim().is_empty()).then_some(slug);
    let outcome = setup_worktree(
        &ProcessTools,
        Path::new(repo_root),
        &claim,
        slug,
        base,
        &mut recorder,
    )?;
    println!("{}", outcome.line());
    Ok(())
}

#[cfg(test)]
mod tests;
