//! git and gh in the agent's cwd, shared by `sm doc` and `sm ticket` / `sm pr`.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// git and gh, behind a seam so resolution is testable without a repo.
pub(crate) trait DocTools {
    fn run(&self, program: &str, cwd: &Path, args: &[&str]) -> Result<ToolOutput>;
}

pub(crate) struct ProcessTools;

impl DocTools for ProcessTools {
    fn run(&self, program: &str, cwd: &Path, args: &[&str]) -> Result<ToolOutput> {
        let output = process::Command::new(program)
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("failed to run {program}"))?;
        Ok(ToolOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

pub(crate) fn run_ok(
    tools: &dyn DocTools,
    program: &str,
    cwd: &Path,
    args: &[&str],
) -> Result<String> {
    let output = tools.run(program, cwd, args)?;
    if !output.success {
        bail!("{program} {} failed: {}", args.join(" "), output.stderr);
    }
    Ok(output.stdout)
}

/// `owner/name` from an `origin` URL: SSH, scp-style, or HTTPS. The host
/// must be exactly `github.com`; anything else falls back to `gh repo view`.
pub(crate) fn parse_github_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let (authority, rest) = if let Some((_, after_scheme)) = url.split_once("://") {
        after_scheme.split_once('/')?
    } else {
        // scp-style: [user@]host:owner/name
        url.split_once(':')?
    };
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !owner.is_empty() && !name.is_empty())
        .then(|| format!("{owner}/{name}"))
}

pub(crate) fn resolve_repo_slug(tools: &dyn DocTools, root: &Path) -> Result<String> {
    if let Ok(url) = run_ok(tools, "git", root, &["remote", "get-url", "origin"]) {
        if let Some(repo) = parse_github_remote(&url) {
            return Ok(repo);
        }
    }
    let repo = run_ok(
        tools,
        "gh",
        root,
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ],
    )
    .context("could not determine the GitHub repo for this checkout")?;
    if repo.is_empty() {
        bail!("could not determine the GitHub repo for this checkout");
    }
    Ok(repo)
}
