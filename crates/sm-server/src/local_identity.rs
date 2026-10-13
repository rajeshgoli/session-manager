//! Verified request identity. This is scoped to one HTTP dispatch, never a
//! process-wide flag or a caller-provided session header.
use crate::local_egress::gateway::VerifiedLocalAgent;
use std::future::Future;

tokio::task_local! { static CALLER: VerifiedLocalAgent; }

pub fn current() -> Option<VerifiedLocalAgent> {
    CALLER.try_with(Clone::clone).ok()
}

pub(crate) async fn scope<T>(identity: VerifiedLocalAgent, future: impl Future<Output = T>) -> T {
    CALLER.scope(identity, future).await
}

/// Carries the request's identity onto a blocking thread, which a task-local
/// does not reach on its own.
pub(crate) fn sync_scope<T>(identity: Option<VerifiedLocalAgent>, f: impl FnOnce() -> T) -> T {
    match identity {
        Some(identity) => CALLER.sync_scope(identity, f),
        None => f(),
    }
}
