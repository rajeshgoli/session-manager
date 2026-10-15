//! Validate whole-set recipes and preserve each successful build commit.
use super::*;
use anyhow::Context;
use std::collections::BTreeSet;
use submission::git_in;
pub fn preserve(
    conn: &Connection,
    config: &ManagedArtifactsConfig,
    root: &ArtifactRoot,
    names: &[String],
) -> Result<()> {
    if !conn.is_autocommit() {
        bail!("preserve recipe tags outside database transactions");
    }
    let first = names.first().context("name at least one artifact")?;
    let a = artifact(conn, &root.path, first)?;
    let set = a["set_id"].as_str().context("set")?;
    let members = store::query(
        conn,
        "SELECT * FROM artifacts WHERE root=?1 AND set_id=?2",
        &[&root.path, &set],
    )?;
    let expected: BTreeSet<_> = members
        .iter()
        .map(|a| a["name"].as_str().unwrap().to_ascii_lowercase())
        .collect();
    let actual: BTreeSet<_> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
    if expected != actual {
        bail!(
            "recipe-complete requires the whole set: {}",
            expected.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    let mut commits = std::collections::BTreeMap::new();
    for a in members {
        if a["origin"] != "queue" {
            bail!("{} is not queue-built", a["name"]);
        }
        let name = a["name"].as_str().unwrap();
        let jobs=store::query(conn,"SELECT * FROM artifact_jobs WHERE root=?1 AND name=?2 AND state='succeeded' ORDER BY seq",&[&root.path,&name])?;
        if jobs.is_empty() {
            bail!("{name} has no succeeded recipe jobs");
        }
        for job in jobs {
            let sha = job["commit_sha"]
                .as_str()
                .context("recipe job has no commit")?;
            if job["dirty"] != 0 {
                bail!("recipe job {} used a dirty worktree", job["job_id"]);
            }
            if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("invalid recipe commit");
            }
            commits
                .entry(sha.to_owned())
                .or_insert_with(Vec::new)
                .push(job["cwd"].as_str().unwrap_or("").to_owned());
        }
    }
    let mirror = crate::sessions::expand_home(&config.mirror_dir)
        .join(format!("{}.git", root.repo.replace('/', "__")));
    // The artifact root owns the recovery repository. A build's checkout may
    // supply an object, but must never choose where its preservation tag lives.
    let checkout = crate::sessions::expand_home(&root.repo_checkout);
    let target = [&checkout, &mirror]
        .into_iter()
        .find_map(|path| {
            let remote = git_in(path, &["remote", "get-url", "origin"]).ok()?;
            if remote.is_empty() {
                return None;
            }
            // Resolve relative local remotes against the configured checkout, not
            // the source checkout from which Git will push the object.
            let local = path.join(&remote);
            Some(
                std::fs::canonicalize(local)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(remote),
            )
        })
        .context("configured artifact repository has no available origin")?;
    for (sha, cwds) in commits {
        // Encode ref-unsafe bytes, including dots (Git rejects .lock and ..).
        let encoded: String = set
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                    char::from(b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let tag = format!("artifact/{encoded}/{}", &sha[..7]);
        let mut candidates = cwds
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>();
        candidates.push(mirror.clone());
        let source = candidates
            .iter()
            .find(|path| {
                path.exists()
                    && git_in(path, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok()
            })
            .context(format!(
                "commit {} is not available to push; rebuild from a pushed commit",
                &sha[..7]
            ))?;
        let remote = git_in(
            source,
            &["ls-remote", "--tags", &target, &format!("refs/tags/{tag}")],
        )?;
        if remote.is_empty() {
            git_in(
                source,
                &["push", &target, &format!("{sha}:refs/tags/{tag}")],
            )?;
        } else if remote.split_whitespace().next() != Some(sha.as_str()) {
            bail!("tag {tag} points at another commit");
        }
        let verified = git_in(
            source,
            &["ls-remote", "--tags", &target, &format!("refs/tags/{tag}")],
        )?;
        if verified.split_whitespace().next() != Some(sha.as_str()) {
            bail!("tag {tag} was not preserved in the configured artifact repository");
        }
        conn.execute("UPDATE artifact_jobs SET tag=?3 WHERE root=?1 AND commit_sha=?2 AND name IN (SELECT name FROM artifacts WHERE root=?1 AND set_id=?4)",params![root.path,sha,tag,set])?;
        event(
            conn,
            &root.path,
            first,
            "tag_preserved",
            "sm",
            json!({"tag":tag}),
        )?;
    }
    Ok(())
}
