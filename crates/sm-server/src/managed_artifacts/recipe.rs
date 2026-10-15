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
    for (sha, cwds) in commits {
        let tag = format!("artifact/{set}/{}", &sha[..7]);
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
            &["ls-remote", "--tags", "origin", &format!("refs/tags/{tag}")],
        )?;
        if remote.is_empty() {
            git_in(
                source,
                &["push", "origin", &format!("{sha}:refs/tags/{tag}")],
            )?;
        } else if remote.split_whitespace().next() != Some(sha.as_str()) {
            bail!("tag {tag} points at another commit");
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
