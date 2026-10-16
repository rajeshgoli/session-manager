//! GitHub/board adapter for durable rollback and cleanup work.
use super::*;
use crate::autopm::recovery::{self, Job};

struct Runtime<'a>(&'a AppState);
impl recovery::Runtime for Runtime<'_> {
    fn file(&self, job: &Job) -> anyhow::Result<i64> {
        actions::file(
            self.0,
            &job.key,
            autopm::REPO,
            &job.title,
            &job.body,
            "autopm",
        )
    }
    fn attach(&self, number: i64, parent: i64) -> anyhow::Result<()> {
        actions::attach(self.0, number, parent).map_err(|e| anyhow::anyhow!("{e:?}"))
    }
    fn start(&self, number: i64, job: &Job) -> anyhow::Result<()> {
        // Acceptance is durable in the board row. Never reset its attempts or
        // revive a failed/cancelled launch on a subsequent recovery tick.
        if board::board_store(self.0)
            .auto_starts()?
            .iter()
            .any(|r| r.repo == autopm::REPO && r.number == number)
        {
            return Ok(());
        }
        actions::authorize(
            self.0,
            &actions::Start {
                number,
                experiment_id: None,
                provider: "codex-fork".into(),
                model: Some("gpt-6-sol".into()),
                reasoning_effort: Some("medium".into()),
                brief: Some(job.body.clone()),
                points_est: 2.0,
            },
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))
    }
    fn merged(&self, number: i64) -> anyhow::Result<bool> {
        let store = store(self.0);
        let settings = settings(self.0).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let context = ctx(self.0, &store, &settings);
        for pr in autopm::linked_prs(&store.open()?, number)? {
            if context.git.pull(pr).map_err(anyhow::Error::msg)?.state == "merged" {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn effects(&self, effects: Vec<Effect>) -> anyhow::Result<()> {
        run_effects_checked(self.0, effects)
    }
}

pub(super) fn tick(state: &AppState) -> anyhow::Result<()> {
    if !state.config.rust_core.runtime_enabled {
        return Ok(());
    }
    // Effects can arise inside the serialized ask path. Its next minute tick
    // retries recovery; never recursively acquire the same lock.
    let _guard = match actions::WORK.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
    };
    let result = recovery::tick(&store(state), autopm::now_ms(), &Runtime(state));
    state.autopm_switches.reload(&store(state).open()?)?;
    result
}

pub(in crate::http) fn is_revert(
    state: &AppState,
    repo: &str,
    number: i64,
) -> anyhow::Result<bool> {
    Ok(repo == autopm::REPO
        && autopm::fix(&store(state).open()?, number)?
            .is_some_and(|f| f.kind == "revert" && f.cancelled_at_ms.is_none()))
}

pub(super) fn current(state: &AppState, number: i64) -> anyhow::Result<bool> {
    let conn = store(state).open()?;
    recovery::schema(&conn)?;
    let saved: Option<(String, bool)> = conn
        .query_row(
            "SELECT payload,done FROM autopm_recovery WHERE number=?1",
            [number],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match saved {
        Some((payload, done)) => {
            Ok(!done
                && recovery::current(&conn, &serde_json::from_str(&payload)?, autopm::now_ms())?)
        }
        None => Ok(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopm::recovery::Runtime as _;

    #[test]
    fn recovery_does_not_reset_accepted_failed_or_cancelled_launches() {
        let state = super::super::tests::role_state();
        let board = board::board_store(&state);
        board.ensure_schema().unwrap();
        board
            .authorize_auto_starts(
                &[crate::board::auto_start::Choice {
                    repo: autopm::REPO.into(),
                    number: 9000,
                    agent_type: None,
                    provider: "codex-fork".into(),
                    model: Some("gpt-6-sol".into()),
                    reasoning_effort: Some("medium".into()),
                    brief: None,
                }],
                time::OffsetDateTime::now_utc(),
            )
            .unwrap();
        let job = Job {
            key: "test".into(),
            experiment: "test".into(),
            mode: "revert".into(),
            epoch: 1,
            parent: 3000,
            title: "test".into(),
            body: "test".into(),
        };
        for status in ["waiting", "started", "failed", "cancelled"] {
            store(&state)
                .open()
                .unwrap()
                .execute(
                    "UPDATE auto_starts SET state=?1,attempts=3 WHERE number=9000",
                    [status],
                )
                .unwrap();
            let before = board.auto_starts().unwrap();
            Runtime(&state).start(9000, &job).unwrap();
            assert_eq!(board.auto_starts().unwrap(), before);
        }
    }
}
