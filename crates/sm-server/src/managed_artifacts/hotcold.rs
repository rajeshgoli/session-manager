//! Retention evaluation. Network failures never turn an unverifiable consumer
//! cold; a failed mirror refresh retains the last complete named-reference scan.
use super::*;
use crate::work_claims::{ItemFetch, WorkItemSource, MAX_ALIASES_PER_QUERY};
use anyhow::{ensure, Context};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn is_hot(
    conn: &Connection,
    root: &str,
    name: &str,
    days: i64,
    at: OffsetDateTime,
) -> Result<bool> {
    let a = artifact(conn, root, name)?;
    let (today, _) = attribution::local_calendar(at)?;
    if a["state"] != "live"
        || a["project"] == 1
        || a["keep_until"]
            .as_str()
            .is_some_and(|s| s >= today.as_str())
        || a["keep_live_until"]
            .as_str()
            .is_some_and(|s| OffsetDateTime::parse(s, &Rfc3339).map_or(true, |t| t > at))
        || ensure_not_in_use(conn, root, name).is_err()
    {
        return Ok(true);
    }
    for field in ["last_used_at", "thawed_at"] {
        if let Some(value) = a[field].as_str() {
            if OffsetDateTime::parse(value, &Rfc3339)
                .map_or(true, |t| at - t < time::Duration::days(days))
            {
                return Ok(true);
            }
        }
    }
    let unknown:i64=conn.query_row("SELECT COUNT(*) FROM artifact_consumers WHERE root=?1 AND name=?2 AND COALESCE(last_state,'UNKNOWN')!='CLOSED'",params![root,name],|r|r.get(0))?;
    let named: i64 = conn.query_row(
        "SELECT COUNT(*) FROM artifact_signals WHERE root=?1 AND name=?2 AND kind='named'",
        params![root, name],
        |r| r.get(0),
    )?;
    // A brand-new mirror with no successful scan has no evidence of absence.
    Ok(unknown > 0 || named > 0 || store::meta(conn, &format!("named_scanned:{root}"))?.is_none())
}

fn git(dir: Option<&Path>, args: &[String], empty_match: bool) -> Result<String> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    command.args(args).env("GIT_TERMINAL_PROMPT", "0");
    let output =
        crate::child_output::output_with_timeout(command, std::time::Duration::from_secs(120))
            .map_err(anyhow::Error::msg)?;
    ensure!(
        output.status.success() || (empty_match && output.status.code() == Some(1)),
        "artifact mirror command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}
fn mirror_path(manager: &ManagedArtifacts, root: &ArtifactRoot) -> PathBuf {
    crate::sessions::expand_home(&manager.config.mirror_dir)
        .join(format!("{}.git", root.repo.replace('/', "__")))
}
pub fn refresh_named(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
) -> Result<BTreeMap<String, Vec<String>>> {
    crate::owner_docs::validate_repo_slug(&root.repo)?;
    let mirror = mirror_path(manager, root);
    fs::create_dir_all(mirror.parent().context("mirror parent")?)?;
    if !mirror.exists() {
        git(
            None,
            &[
                "clone".into(),
                "--bare".into(),
                "--filter=blob:none".into(),
                format!("https://github.com/{}.git", root.repo),
                mirror.display().to_string(),
            ],
            false,
        )?;
    }
    git(
        Some(&mirror),
        &[
            "fetch".into(),
            "origin".into(),
            format!(
                "+refs/heads/{}:refs/heads/{}",
                root.default_branch, root.default_branch
            ),
        ],
        false,
    )?;
    prefetch_tree(&mirror, &root.default_branch)?;
    named_from_mirror(manager, root, &mirror)
}

fn prefetch_tree(mirror: &Path, branch: &str) -> Result<()> {
    // A partial clone otherwise performs one network fetch per missing blob
    // during grep. Fetch only the current tree's missing objects in one batch.
    let objects = git(
        Some(mirror),
        &[
            "rev-list".into(),
            "--objects".into(),
            "--missing=print".into(),
            format!("{branch}^{{tree}}"),
        ],
        false,
    )?;
    let missing: Vec<_> = objects
        .lines()
        .filter_map(|line| line.strip_prefix('?'))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    ensure!(
        missing
            .iter()
            .all(|id| matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit())),
        "invalid missing object id"
    );
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(mirror)
        .args([
            "-c",
            "fetch.negotiationAlgorithm=noop",
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--recurse-submodules=no",
            "--filter=blob:none",
            "origin",
            "--stdin",
        ])
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = crate::child_output::output_with_timeout_input(
        command,
        format!("{}\n", missing.join("\n")).as_bytes(),
        std::time::Duration::from_secs(120),
    )
    .map_err(anyhow::Error::msg)?;
    ensure!(
        output.status.success(),
        "artifact mirror prefetch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
fn named_from_mirror(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    mirror: &Path,
) -> Result<BTreeMap<String, Vec<String>>> {
    let segment = Path::new(&root.path)
        .file_name()
        .and_then(|n| n.to_str())
        .context("root segment")?;
    let mut args = vec![
        "grep".into(),
        "-I".into(),
        "-i".into(),
        "-o".into(),
        "-z".into(),
        "-E".into(),
        format!("{}/[A-Za-z0-9][A-Za-z0-9_.-]*", regex::escape(segment)),
        root.default_branch.clone(),
        "--".into(),
        ".".into(),
    ];
    args.extend(root.named_exclude.iter().map(|s| format!(":!{s}")));
    let output = git(Some(mirror), &args, true)?;
    let conn = manager.store.open()?;
    let mut names = BTreeMap::new();
    for a in manager.store.list(&root.path)? {
        let name = a["name"].as_str().context("name")?;
        names.insert(name.to_lowercase(), name.to_owned());
    }
    for a in store::query(
        &conn,
        "SELECT alias,target_name FROM artifact_aliases WHERE root=?1",
        &[&root.path],
    )? {
        names.insert(
            a["alias"].as_str().context("alias")?.to_lowercase(),
            a["target_name"].as_str().context("target")?.to_owned(),
        );
    }
    let mut matches: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in output.lines() {
        let Some((file, value)) = line.split_once('\0') else {
            continue;
        };
        let Some((_, name)) = value.rsplit_once('/') else {
            continue;
        };
        if let Some(name) = names.get(&name.to_lowercase()) {
            let evidence = file
                .strip_prefix(&format!("{}:", root.default_branch))
                .unwrap_or(file)
                .to_owned();
            let rows = matches.entry(name.clone()).or_default();
            if rows.len() < 20 && !rows.contains(&evidence) {
                rows.push(evidence);
            }
        }
    }
    Ok(matches)
}

/// Fetch outside SQLite write transactions. Apply only complete mirror scans;
/// missing GitHub results explicitly become UNKNOWN rather than retaining CLOSED.
pub fn evaluate(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    source: &dyn WorkItemSource,
) -> Result<()> {
    let named = refresh_named(manager, root);
    evaluate_with_named(manager, root, source, named)
}
fn evaluate_with_named(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    source: &dyn WorkItemSource,
    named: Result<BTreeMap<String, Vec<String>>>,
) -> Result<()> {
    let mut conn = manager.store.open()?;
    let mut consumers: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
    for c in store::query(
        &conn,
        "SELECT repo,number FROM artifact_consumers WHERE root=?1",
        &[&root.path],
    )? {
        consumers
            .entry(c["repo"].as_str().context("consumer repo")?.into())
            .or_default()
            .insert(c["number"].as_i64().context("consumer number")?);
    }
    let mut states = Vec::new();
    for (repo, numbers) in consumers {
        let numbers: Vec<_> = numbers.into_iter().collect();
        for batch in numbers.chunks(MAX_ALIASES_PER_QUERY) {
            let response = source.fetch(&repo, batch);
            for number in batch {
                let state = match response.as_ref().ok().and_then(|r| r.get(number)) {
                    Some(ItemFetch::Found(item))
                        if matches!(item.state.as_str(), "closed" | "merged") =>
                    {
                        "CLOSED"
                    }
                    Some(ItemFetch::Found(item)) if item.state == "open" => "OPEN",
                    _ => "UNKNOWN",
                };
                states.push((repo.clone(), *number, state));
            }
        }
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let at = now();
    for (repo, number, state) in states {
        tx.execute("UPDATE artifact_consumers SET last_state=?4,checked_at=?5 WHERE root=?1 AND repo=?2 AND number=?3",params![root.path,repo,number,state,at])?;
    }
    if let Ok(named) = named {
        tx.execute(
            "DELETE FROM artifact_signals WHERE root=?1 AND kind='named'",
            [&root.path],
        )?;
        for (name, files) in named {
            for file in files {
                tx.execute("INSERT INTO artifact_signals(root,name,kind,evidence,evaluated_at) VALUES(?1,?2,'named',?3,?4)",params![root.path,name,file,at])?;
            }
        }
        store::set_meta(&tx, &format!("named_scanned:{}", root.path), &at)?;
    }
    // in_use is always derived from current reservations/jobs on read; retaining
    // an old persisted row would pin an artifact after its job completed.
    tx.execute(
        "DELETE FROM artifact_signals WHERE root=?1 AND kind IN ('input_of','in_use')",
        [&root.path],
    )?;
    tx.execute("INSERT INTO artifact_signals(root,name,kind,evidence,evaluated_at) SELECT DISTINCT i.root,i.input,'input_of',i.name,?2 FROM artifact_inputs i JOIN artifacts a ON a.root=i.root AND a.name=i.name WHERE i.root=?1 AND a.state!='deleted'",params![root.path,at])?;
    store::set_meta(
        &tx,
        &format!("cold_days:{}", root.path),
        &manager.config.cold_after_days.to_string(),
    )?;
    store::set_meta(&tx, &format!("evaluated:{}", root.path), &at)?;
    tx.commit()?;
    Ok(())
}

pub fn nightly_due(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    at: OffsetDateTime,
) -> Result<bool> {
    let (today, clock) = attribution::local_calendar(at)?;
    Ok(clock >= manager.config.nightly_time
        && store::meta(&manager.store.open()?, &format!("cold_night:{}", root.path))?.as_deref()
            != Some(&today))
}
/// Largest sets first. Every member must qualify; operation claims recheck use
/// and marks, including again immediately before committing the freeze.
pub fn freeze_candidates(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    at: OffsetDateTime,
) -> Result<Vec<String>> {
    if !moves::cold_mounted(root) {
        return Ok(Vec::new());
    }
    let conn = manager.store.open()?;
    let mut sets: BTreeMap<String, (String, u64, bool)> = BTreeMap::new();
    for a in manager.store.list(&root.path)? {
        let name = a["name"].as_str().context("name")?;
        let expired = a["state"] == "unrecorded"
            && a["grace_until"]
                .as_str()
                .is_some_and(|s| OffsetDateTime::parse(s, &Rfc3339).is_ok_and(|t| t <= at));
        let cold = !is_hot(&conn, &root.path, name, manager.config.cold_after_days, at)?;
        let eligible = (cold || expired)
            && a["op"].is_null()
            && ensure_not_in_use(&conn, &root.path, name).is_ok();
        let row = sets
            .entry(a["set_id"].as_str().context("set")?.into())
            .or_insert((name.into(), 0, true));
        row.1 = row.1.saturating_add(a["disk_bytes"].as_u64().unwrap_or(0));
        row.2 &= eligible;
    }
    let mut sets: Vec<_> = sets.into_values().filter(|r| r.2).collect();
    sets.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(sets.into_iter().map(|r| r.0).collect())
}
pub fn nightly(
    manager: &ManagedArtifacts,
    root: &ArtifactRoot,
    source: &dyn WorkItemSource,
) -> Result<()> {
    let at = OffsetDateTime::now_utc();
    if !nightly_due(manager, root, at)? {
        return Ok(());
    }
    evaluate(manager, root, source)?;
    for name in freeze_candidates(manager, root, at)? {
        if let Err(error) = ops::request(manager, root, &name, "freezing", "sm-nightly") {
            eprintln!("nightly freeze skipped {name}: {error:#}");
        }
    }
    let (today, _) = attribution::local_calendar(at)?;
    store::set_meta(
        &manager.store.open()?,
        &format!("cold_night:{}", root.path),
        &today,
    )?;
    ops::kick(manager.clone());
    Ok(())
}

#[cfg(test)]
mod tests;
