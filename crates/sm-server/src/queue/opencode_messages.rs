//! Stable provider identities, separate from message text and delivery effects.
use super::*;
use crate::opencode::MessageBinding;

impl RetainedQueueStore {
    /// Include delivered rows: a reconnect may replay the host's earlier input.
    pub fn opencode_generated_message_ids(
        &self,
        target: &str,
        conversation: &str,
    ) -> Result<BTreeSet<String>> {
        crate::opencode::validate_id(conversation, "ses")?;
        self.with_connection(|conn| {
            let mut statement = conn.prepare("SELECT provider_message_id FROM message_queue WHERE target_session_id=?1 AND provider_conversation_id=?2 AND provider_message_id IS NOT NULL")?;
            let rows = statement.query_map(params![target, conversation], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<BTreeSet<_>>>()?)
        })
    }

    /// Atomically assign all three identities, or return the existing binding.
    /// The caller holds the runtime's input lock and calls this immediately
    /// before its first delivery attempt, so IDs reflect submission order.
    pub fn bind_pending_provider_message(
        &self,
        target_session_id: &str,
        message_id: &str,
        conversation_id: &str,
    ) -> Result<MessageBinding> {
        let candidate = MessageBinding::new(conversation_id)?;
        self.with_connection(|conn| {
            // The first statement takes the SQLite write lock. No read-then-write
            // race can replace the winner's identity on another connection.
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "UPDATE message_queue SET provider_message_id = ?3, provider_part_id = ?4,
                    provider_conversation_id = ?5
                 WHERE id = ?1 AND target_session_id = ?2 AND delivered_at IS NULL
                    AND provider_message_id IS NULL AND provider_part_id IS NULL
                    AND provider_conversation_id IS NULL",
                params![
                    message_id,
                    target_session_id,
                    candidate.message_id,
                    candidate.part_id,
                    candidate.conversation_id
                ],
            )?;
            let binding = read_binding(&tx, target_session_id, message_id)?
                .context("pending message has no provider binding")?;
            tx.commit()?;
            Ok(binding)
        })
    }

    /// None means an unbound pending row. Missing, delivered and wrong-target
    /// rows are errors, never authoritative absence from the provider.
    pub fn pending_provider_message_binding(
        &self,
        target_session_id: &str,
        message_id: &str,
    ) -> Result<Option<MessageBinding>> {
        self.with_connection(|conn| read_binding(conn, target_session_id, message_id))
    }

    /// Call only after GET on expected.conversation_id returned 404. Clear and
    /// handoff use this compare-and-clear before moving to a new conversation.
    /// An unreachable provider must leave these identities intact.
    pub fn clear_pending_provider_message_binding(
        &self,
        target_session_id: &str,
        message_id: &str,
        expected: &MessageBinding,
    ) -> Result<bool> {
        expected.validate()?;
        self.with_connection(|conn| {
            Ok(conn.execute(
                "UPDATE message_queue SET provider_message_id = NULL, provider_part_id = NULL,
                provider_conversation_id = NULL
             WHERE id = ?1 AND target_session_id = ?2 AND delivered_at IS NULL
                AND provider_message_id = ?3 AND provider_part_id = ?4
                AND provider_conversation_id = ?5",
                params![
                    message_id,
                    target_session_id,
                    expected.message_id,
                    expected.part_id,
                    expected.conversation_id
                ],
            )? == 1)
        })
    }
}

fn read_binding(conn: &Connection, target: &str, id: &str) -> Result<Option<MessageBinding>> {
    let (message, part, conversation): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT provider_message_id, provider_part_id, provider_conversation_id
             FROM message_queue WHERE id = ?1 AND target_session_id = ?2 AND delivered_at IS NULL",
            params![id, target],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .context("pending message not found for target")?;
    match (message, part, conversation) {
        (None, None, None) => Ok(None),
        (Some(message_id), Some(part_id), Some(conversation_id)) => {
            let binding = MessageBinding {
                message_id,
                part_id,
                conversation_id,
            };
            binding.validate()?;
            Ok(Some(binding))
        }
        _ => bail!("pending message has an incomplete provider binding"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::tests::ScratchDir;

    #[test]
    fn opencode_generated_ids_include_delivered_rows_and_isolate_target_and_conversation() {
        let tmp = ScratchDir::new();
        let path = tmp.path().join("queue.db");
        let store = RetainedQueueStore::new(path.clone());
        let message = store
            .enqueue_message("agent", "hello", "sequential", None)
            .unwrap();
        let binding = store
            .bind_pending_provider_message("agent", &message, "ses_test")
            .unwrap();
        store
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE message_queue SET delivered_at='2026-10-07T00:00:00Z' WHERE id=?1",
                    [&message],
                )?;
                Ok(())
            })
            .unwrap();
        let reopened = RetainedQueueStore::new(path);
        assert_eq!(
            reopened
                .opencode_generated_message_ids("agent", "ses_test")
                .unwrap(),
            BTreeSet::from([binding.message_id])
        );
        assert!(reopened
            .opencode_generated_message_ids("other", "ses_test")
            .unwrap()
            .is_empty());
        assert!(reopened
            .opencode_generated_message_ids("agent", "ses_new")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn opencode_binding_survives_reopen_and_compare_clear_preserves_replacement() {
        let temp = ScratchDir::new();
        let path = temp.path().join("queue.db");
        let queue = RetainedQueueStore::new(path.clone());
        let id = queue
            .enqueue_message("agent", "hello", "urgent", None)
            .unwrap();
        let first = queue
            .bind_pending_provider_message("agent", &id, "ses_old")
            .unwrap();
        let reopened = RetainedQueueStore::new(path);
        assert_eq!(
            first,
            reopened
                .bind_pending_provider_message("agent", &id, "ses_new")
                .unwrap()
        );
        assert!(!reopened
            .clear_pending_provider_message_binding("other", &id, &first)
            .unwrap());
        assert!(reopened
            .clear_pending_provider_message_binding("agent", &id, &first)
            .unwrap());
        let second = reopened
            .bind_pending_provider_message("agent", &id, "ses_new")
            .unwrap();
        assert_ne!(first.message_id, second.message_id);
        assert_eq!(second.conversation_id, "ses_new");
        assert!(!reopened
            .clear_pending_provider_message_binding("agent", &id, &first)
            .unwrap());
        assert_eq!(
            reopened
                .pending_provider_message_binding("agent", &id)
                .unwrap(),
            Some(second)
        );
        assert!(reopened
            .bind_pending_provider_message("other", &id, "ses_new")
            .is_err());
    }

    #[test]
    fn opencode_binding_has_one_winner_on_concurrent_connections() {
        let temp = ScratchDir::new();
        let queue = Arc::new(RetainedQueueStore::new(temp.path().join("queue.db")));
        let id = queue
            .enqueue_message("agent", "hello", "sequential", None)
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (queue, id, barrier) = (queue.clone(), id.clone(), barrier.clone());
                thread::spawn(move || {
                    barrier.wait();
                    queue
                        .bind_pending_provider_message("agent", &id, "ses_old")
                        .unwrap()
                })
            })
            .collect();
        let bindings: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(bindings.iter().all(|binding| binding == &bindings[0]));
    }

    #[test]
    fn opencode_migration_keeps_hosted_rows_unbound_and_rejects_partial_identity() {
        let temp = ScratchDir::new();
        let path = temp.path().join("queue.db");
        let queue = RetainedQueueStore::new(path.clone());
        let id = queue
            .enqueue_message("claude", "legacy text", "sequential", None)
            .unwrap();
        let conn = Connection::open(&path).unwrap();
        for column in [
            "provider_message_id",
            "provider_part_id",
            "provider_conversation_id",
        ] {
            conn.execute(
                &format!("ALTER TABLE message_queue DROP COLUMN {column}"),
                [],
            )
            .unwrap();
        }
        let reopened = RetainedQueueStore::new(path);
        assert_eq!(
            reopened
                .pending_provider_message_binding("claude", &id)
                .unwrap(),
            None
        );
        assert_eq!(
            reopened.pending_messages_for_target("claude", 10).unwrap()[0].text,
            "legacy text"
        );
        conn.execute(
            "UPDATE message_queue SET provider_message_id = 'msg_broken' WHERE id = ?1",
            [&id],
        )
        .unwrap();
        assert!(reopened
            .bind_pending_provider_message("claude", &id, "ses_old")
            .is_err());
        assert!(reopened
            .pending_provider_message_binding("claude", &id)
            .is_err());
    }
}
