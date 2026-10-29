//! `sm pr findings` and `sm pr merge` (#2605): the PR's open P0/P1 review
//! findings, and a merge that refuses while any is open unless each one is
//! given a reason, which sm posts on the PR before merging.

use super::*;
use sm_server::review::findings::{self, Finding, PrFindings};

/// `sm pr merge` refused: open findings without a reason.
pub(crate) const EXIT_OPEN_FINDINGS: i32 = 4;

#[derive(Subcommand)]
pub(crate) enum PrCommand {
    /// List the PR's P0/P1 review findings nobody answered
    Findings(FindingsArgs),
    /// Merge the PR; refuses while a P0/P1 finding is unanswered
    Merge(MergeArgs),
}

#[derive(Args)]
pub(crate) struct FindingsArgs {
    /// PR number; without one, the current branch's open PR
    number: Option<i64>,
    /// owner/name, or a bare repo name with the cwd repo's owner
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(crate) struct MergeArgs {
    /// PR number; without one, the current branch's open PR
    number: Option<i64>,
    /// owner/name, or a bare repo name with the cwd repo's owner
    #[arg(long)]
    repo: Option<String>,
    /// Merge over one open finding: FINDING_ID=why it stays open (repeatable)
    #[arg(long = "reason", value_name = "ID=REASON")]
    reasons: Vec<String>,
    /// Arguments for `gh pr merge` (default: --squash)
    #[arg(last = true)]
    gh_args: Vec<String>,
}

pub(crate) fn run(command: PrCommand) -> Result<()> {
    match command {
        PrCommand::Findings(args) => {
            let (_, number, found) = load(args.number, args.repo.as_deref())?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&to_json(&found))?);
            } else {
                println!("{}", list_text(number, &found));
            }
            Ok(())
        }
        PrCommand::Merge(args) => merge(args),
    }
}

fn load(number: Option<i64>, repo: Option<&str>) -> Result<(String, i64, PrFindings)> {
    let cwd = env::current_dir()?;
    let target = resolve_claim_target(&ProcessTools, &cwd, repo)?;
    let number = match number {
        Some(number) => number,
        None => current_branch_pr(&ProcessTools, &cwd)?,
    };
    let found = findings::fetch(&target.repo, number, &findings::gh_graphql).map_err(|error| {
        anyhow!(
            "reading review findings on {}#{number}: {error}",
            target.repo
        )
    })?;
    Ok((target.repo, number, found))
}

fn to_json(found: &PrFindings) -> Value {
    json!({
        "state": found.state,
        "url": found.url,
        "open": found.open.iter().map(|f| json!({
            "id": f.id, "severity": format!("P{}", f.severity), "path": f.path,
            "line": f.line, "title": f.title, "url": f.url, "commit": f.commit,
            "created_at": f.created_at,
        })).collect::<Vec<_>>(),
        "lower_open": found.lower_open,
    })
}

pub(crate) fn list_text(number: i64, found: &PrFindings) -> String {
    let lower = match found.lower_open {
        0 => String::new(),
        n => format!(" ({n} P2-or-lower not counted)"),
    };
    if found.open.is_empty() {
        return format!("PR #{number}: no open P0/P1 review findings{lower}.");
    }
    let mut out = format!(
        "PR #{number}: {} open P0/P1 review finding{} nobody answered{lower}:",
        found.open.len(),
        if found.open.len() == 1 { "" } else { "s" }
    );
    for f in &found.open {
        let commit = &f.commit[..f.commit.len().min(7)];
        out.push_str(&format!(
            "\n  {}  {}\n      reviewed at {commit}: {}",
            f.id,
            findings::finding_line(f),
            f.url
        ));
    }
    out.push_str(
        "\nA finding closes with a reply on its thread, a resolve, or a PR comment of yours \
         that names its reviewed commit.",
    );
    out
}

/// `ID=reason` pairs, each id at most once and each reason non-empty.
fn parse_reasons(raw: &[String]) -> Result<BTreeMap<String, String>> {
    let mut reasons = BTreeMap::new();
    for entry in raw {
        let Some((id, reason)) = entry.split_once('=') else {
            bail!("--reason takes FINDING_ID=REASON, got {entry:?}");
        };
        let (id, reason) = (id.trim(), reason.trim());
        if reason.is_empty() {
            bail!("--reason for finding {id} is empty");
        }
        if reasons.insert(id.to_owned(), reason.to_owned()).is_some() {
            bail!("--reason given twice for finding {id}");
        }
    }
    Ok(reasons)
}

/// The findings each reason covers, or why the merge must not go ahead.
fn match_reasons<'a>(
    number: i64,
    found: &'a PrFindings,
    reasons: &'a BTreeMap<String, String>,
) -> std::result::Result<Vec<(&'a Finding, &'a str)>, (i32, String)> {
    let unknown: Vec<&str> = reasons
        .keys()
        .filter(|id| !found.open.iter().any(|f| &f.id == *id))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err((
            1,
            format!(
                "--reason names {} that {} not an open P0/P1 finding on PR #{number}.\n{}",
                unknown.join(", "),
                if unknown.len() == 1 { "is" } else { "are" },
                list_text(number, found)
            ),
        ));
    }
    let missing = found
        .open
        .iter()
        .filter(|f| !reasons.contains_key(&f.id))
        .count();
    if missing > 0 {
        return Err((
            EXIT_OPEN_FINDINGS,
            format!(
                "Refusing to merge.\n{}\nTo merge anyway, give a reason for every open finding: \
                 sm pr merge {number} --reason <id>='why it stays open' ... \
                 sm posts the reasons on the PR first.",
                list_text(number, found)
            ),
        ));
    }
    Ok(found
        .open
        .iter()
        .map(|f| (f, reasons[&f.id].as_str()))
        .collect())
}

fn merge(args: MergeArgs) -> Result<()> {
    let reasons = parse_reasons(&args.reasons)?;
    let (repo, number, found) = load(args.number, args.repo.as_deref())?;
    if found.state != "OPEN" {
        bail!("PR #{number} is {}, not open.", found.state.to_lowercase());
    }
    let covered = match match_reasons(number, &found, &reasons) {
        Ok(covered) => covered,
        Err((exit, text)) => {
            eprintln!("{text}");
            process::exit(exit);
        }
    };
    if !covered.is_empty() {
        let session = optional_current_session_id().ok_or_else(|| {
            anyhow!("merging over open findings must run inside a managed session, which signs the reasons")
        })?;
        let body = findings::override_comment(&session, &covered);
        let mut child = process::Command::new("gh")
            .args([
                "pr",
                "comment",
                &number.to_string(),
                "--repo",
                &repo,
                "--body-file",
                "-",
            ])
            .stdin(process::Stdio::piped())
            .spawn()
            .context("running gh pr comment")?;
        child
            .stdin
            .take()
            .context("gh pr comment stdin")?
            .write_all(body.as_bytes())?;
        if !child.wait()?.success() {
            bail!("Posting the override reasons on PR #{number} failed; not merging.");
        }
    }
    let mut command = process::Command::new("gh");
    command.args(["pr", "merge", &number.to_string()]);
    if args.repo.is_some() {
        command.args(["--repo", &repo]);
    }
    if args.gh_args.is_empty() {
        command.arg("--squash");
    } else {
        command.args(&args.gh_args);
    }
    let status = command.status().context("running gh pr merge")?;
    if !status.success() {
        process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(id: &str) -> Finding {
        Finding {
            id: id.into(),
            severity: 1,
            path: "a.rs".into(),
            line: Some(3),
            title: "Fills inverted".into(),
            url: format!("https://x/{id}"),
            commit: "aaaa1111abc".into(),
            created_at: "2026-10-28T16:40:01Z".into(),
        }
    }

    fn found(ids: &[&str]) -> PrFindings {
        PrFindings {
            author: "me".into(),
            state: "OPEN".into(),
            merged_at: None,
            url: String::new(),
            open: ids.iter().map(|id| finding(id)).collect(),
            lower_open: 2,
        }
    }

    #[test]
    fn parses_pr_subcommands_beside_the_claim_number() {
        assert!(Cli::try_parse_from(["sm", "pr", "12"]).is_ok());
        assert!(Cli::try_parse_from(["sm", "pr", "findings", "12", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["sm", "pr", "merge"]).is_ok());
        assert!(Cli::try_parse_from([
            "sm",
            "pr",
            "merge",
            "12",
            "--reason",
            "1=deferred",
            "--",
            "--squash",
            "--delete-branch"
        ])
        .is_ok());
    }

    #[test]
    fn list_names_each_open_finding() {
        let text = list_text(7, &found(&["11"]));
        assert!(text.starts_with(
            "PR #7: 1 open P0/P1 review finding nobody answered (2 P2-or-lower not counted):"
        ));
        assert!(text
            .contains("  11  P1 a.rs:3 — Fills inverted\n      reviewed at aaaa111: https://x/11"));
        let mut none = found(&[]);
        none.lower_open = 0;
        assert_eq!(list_text(7, &none), "PR #7: no open P0/P1 review findings.");
    }

    #[test]
    fn merge_needs_a_reason_for_every_open_finding() {
        let pr = found(&["11", "12"]);
        let one = parse_reasons(&["11=fixed elsewhere".into()]).unwrap();
        let (exit, text) = match_reasons(7, &pr, &one).unwrap_err();
        assert_eq!(exit, EXIT_OPEN_FINDINGS);
        assert!(text.starts_with("Refusing to merge."));
        let typo = parse_reasons(&["11=a".into(), "12=b".into(), "13=c".into()]).unwrap();
        assert_eq!(match_reasons(7, &pr, &typo).unwrap_err().0, 1);
        let both = parse_reasons(&["11=a".into(), "12=b".into()]).unwrap();
        let covered = match_reasons(7, &pr, &both).unwrap();
        assert_eq!(
            covered
                .iter()
                .map(|(f, r)| (f.id.as_str(), *r))
                .collect::<Vec<_>>(),
            [("11", "a"), ("12", "b")]
        );
        assert!(match_reasons(7, &found(&[]), &BTreeMap::new())
            .unwrap()
            .is_empty());
        assert!(parse_reasons(&["11".into()]).is_err());
        assert!(parse_reasons(&["11= ".into()]).is_err());
        assert!(parse_reasons(&["11=a".into(), "11=b".into()]).is_err());
    }
}
