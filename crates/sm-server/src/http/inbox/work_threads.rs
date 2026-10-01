use super::*;
use crate::owner_docs::{DocInboxFacts, OwnerDocPublish};
use crate::work_claims::{WorkClaim, WorkItem};

struct WorkIndex {
    claims: Vec<WorkClaim>,
    items: BTreeMap<(String, i64), WorkItem>,
    links: BTreeMap<(String, i64), Vec<i64>>,
    reader_keys: BTreeMap<String, String>,
}

impl WorkIndex {
    fn load(state: &AppState) -> Result<Self, ApiError> {
        let store = crate::http::claims::work_claim_store(state);
        let mut links: BTreeMap<(String, i64), Vec<i64>> = BTreeMap::new();
        for (repo, pr, ticket) in store.all_links()? {
            links
                .entry((repo.to_ascii_lowercase(), pr))
                .or_default()
                .push(ticket);
        }
        let mut index = Self {
            claims: store.all_claims()?,
            items: store
                .all_items()?
                .into_iter()
                .map(|item| ((item.repo.to_ascii_lowercase(), item.number), item))
                .collect(),
            links,
            reader_keys: BTreeMap::new(),
        };
        let docs = owner_doc_store(state);
        for (session_id, doc_id) in docs.reader_sessions()? {
            let (Some(doc), Some(first)) = (
                docs.get(&doc_id)?,
                docs.publishes(&doc_id)?.into_iter().next(),
            ) else {
                continue;
            };
            index
                .reader_keys
                .insert(session_id, index.doc_key(&doc, &first));
        }
        Ok(index)
    }

    fn claim_key(&self, session_id: &str, at: &str, kind: &str) -> Option<String> {
        let at = parse_ts(at)?;
        self.claims
            .iter()
            .filter(|claim| {
                claim.session_id == session_id
                    && claim.kind == kind
                    && parse_ts(&claim.claimed_at).is_some_and(|start| start <= at)
                    && claim
                        .ended_at
                        .as_deref()
                        .and_then(parse_ts)
                        .is_none_or(|end| at <= end)
            })
            .min_by(|a, b| {
                a.claimed_at
                    .cmp(&b.claimed_at)
                    .then_with(|| a.id.cmp(&b.id))
            })
            .map(|claim| {
                format!(
                    "{}:{}#{}",
                    kind,
                    claim.repo.to_ascii_lowercase(),
                    claim.number
                )
            })
    }

    fn agent_key(&self, session_id: &str, at: &str) -> String {
        self.reader_keys
            .get(session_id)
            .cloned()
            .or_else(|| self.claim_key(session_id, at, "ticket"))
            .or_else(|| self.claim_key(session_id, at, "pr"))
            .unwrap_or_else(|| agent_thread_key(session_id))
    }

    fn doc_key(&self, doc: &crate::owner_docs::OwnerDoc, first: &OwnerDocPublish) -> String {
        if let Some(key) = self.claim_key(&first.session_id, &first.published_at, "ticket") {
            return key;
        }
        let repo = doc.repo.to_ascii_lowercase();
        if let Some(pr) = doc.pr_number.or(first.pr_number) {
            if let Some(ticket) = self
                .links
                .get(&(repo.clone(), pr))
                .and_then(|numbers| numbers.first())
            {
                return format!("ticket:{repo}#{ticket}");
            }
            return format!("pr:{repo}#{pr}");
        }
        format!("docpath:{repo}/{}", doc.path)
    }

    fn title(&self, key: &str) -> Option<String> {
        let (kind, value) = key.split_once(':')?;
        let (repo, number) = value.rsplit_once('#')?;
        let number: i64 = number.parse().ok()?;
        let prefix = if kind == "ticket" {
            format!("#{number}")
        } else {
            format!("PR #{number}")
        };
        Some(match self.items.get(&(repo.to_owned(), number)) {
            Some(item) if !item.title.is_empty() => format!("{prefix} {}", item.title),
            _ => prefix,
        })
    }
}

struct DocData {
    summary: OwnerDocSummary,
    facts: DocInboxFacts,
    publishes: Vec<OwnerDocPublish>,
    key: String,
}

pub(crate) struct ThreadCatalog {
    world: World,
    work: WorkIndex,
    docs: Vec<DocData>,
    ask_answer_keys: BTreeMap<String, String>,
}

/// The first owner message after a delivered Ask is that question's answer.
/// A newer Ask replaces the pending question for the same agent.
fn ask_answer_keys(world: &World) -> BTreeMap<String, String> {
    enum Event<'a> {
        Ask(&'a OwnerNote),
        Answer(&'a OwnerMessage),
    }
    let mut events = Vec::new();
    for note in world
        .notes
        .iter()
        .filter(|note| note.id.starts_with("ask-") && note.thread_key.is_some())
    {
        events.push((note.created_at.as_str(), 0, Event::Ask(note)));
    }
    for message in &world.messages {
        events.push((message.created_at.as_str(), 1, Event::Answer(message)));
    }
    events.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let mut pending: BTreeMap<&str, &OwnerNote> = BTreeMap::new();
    let mut keys = BTreeMap::new();
    for (_, _, event) in events {
        match event {
            Event::Ask(note) => {
                pending.insert(&note.delivered_to_session_id, note);
            }
            Event::Answer(message) => {
                if let Some(note) = pending.remove(message.sender_session_id.as_str()) {
                    if let Some(key) = &note.thread_key {
                        keys.insert(message.id.clone(), key.clone());
                    }
                }
            }
        }
    }
    keys
}

struct ThreadEntry<'a> {
    key: String,
    legacy_key: String,
    sender_id: Option<&'a str>,
    item: Item<'a>,
    counted: bool,
}

fn add_agent_entry<'a>(
    entries: &mut Vec<ThreadEntry<'a>>,
    work: &WorkIndex,
    session_id: &'a str,
    at: &str,
    item: Item<'a>,
    counted: bool,
) {
    entries.push(ThreadEntry {
        key: work.agent_key(session_id, at),
        legacy_key: agent_thread_key(session_id),
        sender_id: Some(session_id),
        item,
        counted,
    });
}

impl ThreadCatalog {
    pub(crate) fn load(state: &AppState) -> Result<Self, ApiError> {
        let world = World::load(state)?;
        let ask_answer_keys = ask_answer_keys(&world);
        let work = WorkIndex::load(state)?;
        let store = owner_doc_store(state);
        let facts = store.inbox_facts()?;
        let mut docs = Vec::new();
        for summary in store.summaries(None, false)? {
            let publishes = store.publishes(&summary.doc.id)?;
            let Some(first) = publishes.first() else {
                continue;
            };
            let key = work.doc_key(&summary.doc, first);
            docs.push(DocData {
                facts: facts.get(&summary.doc.id).cloned().unwrap_or_default(),
                summary,
                publishes,
                key,
            });
        }
        Ok(Self {
            world,
            work,
            docs,
            ask_answer_keys,
        })
    }

    fn message_key(&self, message: &OwnerMessage) -> String {
        self.ask_answer_keys
            .get(&message.id)
            .cloned()
            .unwrap_or_else(|| {
                self.work
                    .agent_key(&message.sender_session_id, &message.created_at)
            })
    }

    fn entries(&self) -> Vec<ThreadEntry<'_>> {
        let mut entries = Vec::new();
        let replied = self.world.replied();
        let senders: BTreeMap<&str, &OwnerMessage> = self
            .world
            .messages
            .iter()
            .map(|m| (m.id.as_str(), m))
            .collect();
        for message in &self.world.messages {
            let key = self.message_key(message);
            entries.push(ThreadEntry {
                key: key.clone(),
                legacy_key: agent_thread_key(&message.sender_session_id),
                sender_id: Some(&message.sender_session_id),
                item: Item::Message(message, self.world.message_state(message, &replied)),
                counted: true,
            });
            if message.handled_at.is_some() {
                entries.push(ThreadEntry {
                    key,
                    legacy_key: agent_thread_key(&message.sender_session_id),
                    sender_id: Some(&message.sender_session_id),
                    item: Item::Answered(message),
                    counted: false,
                });
            }
        }
        for reply in &self.world.replies {
            if let Some(message) = senders.get(reply.message_id.as_str()) {
                // A reply stays with the message it answered, including after the claim ends.
                let key = self.message_key(message);
                entries.push(ThreadEntry {
                    key,
                    legacy_key: agent_thread_key(&message.sender_session_id),
                    sender_id: Some(&message.sender_session_id),
                    item: Item::Reply(reply),
                    counted: true,
                });
            }
        }
        for note in &self.world.notes {
            if let Some(key) = &note.thread_key {
                entries.push(ThreadEntry {
                    key: key.clone(),
                    legacy_key: agent_thread_key(&note.session_id),
                    sender_id: Some(&note.session_id),
                    item: Item::Note(note),
                    counted: true,
                });
            } else {
                add_agent_entry(
                    &mut entries,
                    &self.work,
                    &note.session_id,
                    &note.created_at,
                    Item::Note(note),
                    true,
                );
            }
        }
        for follow in &self.world.follows {
            let at = follow.fired_at.as_deref().unwrap_or(&follow.created_at);
            add_agent_entry(
                &mut entries,
                &self.work,
                &follow.session_id,
                at,
                Item::Follow(follow),
                true,
            );
        }
        for row in &self.world.finished {
            if row.text.is_none() {
                continue;
            }
            let at = row.text_at.as_deref().unwrap_or(&row.completed_at);
            add_agent_entry(
                &mut entries,
                &self.work,
                &row.session_id,
                at,
                Item::Turn(row),
                true,
            );
        }
        for reply in &self.world.agent_replies {
            if self.world.finished.iter().any(|row| {
                row.session_id == reply.session_id
                    && row.text_at.as_deref() == Some(reply.at.as_str())
            }) {
                continue;
            }
            let key = self
                .world
                .replies
                .iter()
                .find(|input| {
                    input.delivered_to_session_id == reply.session_id
                        && norm(&input.created_at) == norm(&reply.input_at)
                })
                .and_then(|input| senders.get(input.message_id.as_str()))
                .map(|message| self.message_key(message))
                .or_else(|| {
                    self.world
                        .notes
                        .iter()
                        .find(|input| {
                            input.delivered_to_session_id == reply.session_id
                                && norm(&input.created_at) == norm(&reply.input_at)
                        })
                        .map(|note| {
                            note.thread_key.clone().unwrap_or_else(|| {
                                self.work.agent_key(&note.session_id, &note.created_at)
                            })
                        })
                })
                .unwrap_or_else(|| self.work.agent_key(&reply.session_id, &reply.input_at));
            entries.push(ThreadEntry {
                key,
                legacy_key: agent_thread_key(&reply.session_id),
                sender_id: Some(&reply.session_id),
                item: Item::AgentReply(reply),
                counted: true,
            });
        }
        for doc in &self.docs {
            for publish in &doc.publishes {
                entries.push(ThreadEntry {
                    key: doc.key.clone(),
                    legacy_key: doc_thread_key(&doc.summary.doc.id),
                    sender_id: Some(&publish.session_id),
                    item: Item::Doc(&doc.summary.doc, publish),
                    counted: true,
                });
            }
        }
        entries.sort_by_key(|entry| entry.item.at());
        entries
    }

    pub(super) fn rows(&self, state: &AppState) -> Result<Vec<InboxRow>, ApiError> {
        let entries = self.entries();
        let mut keys = BTreeSet::new();
        keys.extend(entries.iter().map(|entry| entry.key.clone()));
        let legacy_docs: BTreeMap<String, bool> = self
            .world
            .doc_rows(state)?
            .into_iter()
            .map(|row| (row.thread_key, row.done))
            .collect();
        let mut marks_map = self.world.marks.clone();
        // Old Done marks count items in each agent/doc thread. Walk those items
        // in their old order so a later ticket cannot erase an earlier ticket's
        // migrated Done watermark when one agent's history splits by claim.
        let mut legacy_position: BTreeMap<&str, i64> = BTreeMap::new();
        let mut migrated_items: BTreeMap<&str, usize> = BTreeMap::new();
        for entry in entries.iter().filter(|entry| entry.counted) {
            let position = legacy_position
                .entry(entry.legacy_key.as_str())
                .or_default();
            *position += 1;
            if self
                .world
                .marks
                .get(&entry.legacy_key)
                .and_then(|marks| marks.done_items)
                .is_some_and(|done| *position <= done)
            {
                *migrated_items.entry(entry.key.as_str()).or_default() += 1;
            }
        }
        let mut rows = Vec::new();
        for key in keys {
            let own: Vec<_> = entries.iter().filter(|entry| entry.key == key).collect();
            let mut senders = BTreeSet::new();
            let mut doc_ids = BTreeSet::new();
            let mut items = 0;
            let mut messages = 0;
            let mut revisions = 0;
            let mut open_asks = 0;
            let mut group = "earlier";
            let mut preview = String::new();
            let mut newest_at = String::new();
            let mut group_at = String::new();
            let mut unread_finished = false;
            let marks = marks_map.get(&key).cloned().unwrap_or_default();
            let mut follow_count = 0usize;
            for entry in &own {
                if let Some(sender) = entry.sender_id {
                    senders.insert(sender.to_owned());
                }
                if entry.counted {
                    items += 1;
                }
                let at = entry.item.at();
                newest_at = newest_at.max(at.clone());
                let (candidate, line) = match &entry.item {
                    Item::Message(message, state) => {
                        messages += 1;
                        open_asks += usize::from(*state == OwnerMessageState::NeedsYou);
                        (
                            match state {
                                OwnerMessageState::NeedsYou => "needs_you",
                                OwnerMessageState::New => "new",
                                _ => "earlier",
                            },
                            message.title.clone(),
                        )
                    }
                    Item::Turn(row) => {
                        unread_finished |= row.read_at.is_none();
                        (
                            if row.read_at.is_none() {
                                "finished"
                            } else {
                                "earlier"
                            },
                            first_line(row.text.as_deref().unwrap_or(""), 140),
                        )
                    }
                    Item::AgentReply(reply) => {
                        let unread = marks
                            .last_read_at
                            .as_deref()
                            .is_none_or(|read| norm(&reply.at) > norm(read));
                        (
                            if unread { "new" } else { "earlier" },
                            first_line(&reply.text, 140),
                        )
                    }
                    Item::Doc(doc, _) => {
                        revisions += 1;
                        doc_ids.insert(doc.id.clone());
                        ("earlier", doc.title.clone())
                    }
                    Item::Reply(reply) => ("earlier", format!("You: {}", snippet(&reply.body))),
                    Item::Note(note) => ("earlier", format!("You: {}", snippet(&note.body))),
                    Item::Follow(follow) => {
                        follow_count += 1;
                        let seen = marks
                            .read_follows
                            .and_then(|n| usize::try_from(n).ok())
                            .unwrap_or(0);
                        (
                            if follow_count > seen {
                                "new"
                            } else {
                                "earlier"
                            },
                            follow_line(follow).0,
                        )
                    }
                    Item::Answered(_) => ("earlier", String::new()),
                };
                if group_rank(candidate) < group_rank(group)
                    || (group == candidate && at >= group_at)
                {
                    group = candidate;
                    group_at = at;
                    preview = line;
                }
            }
            let mut status = if senders.iter().any(|id| self.world.live(id)) {
                "live"
            } else {
                "ended"
            }
            .to_owned();
            let mut verdict = None;
            let mut pr_number = None;
            let mut author = None;
            let mut doc_url = None;
            let mut selected_doc_rank = u8::MAX;
            let mut selected_doc_at = String::new();
            for doc in self.docs.iter().filter(|doc| doc.key == key) {
                items += doc.facts.review_count;
                let doc_group = match doc.summary.state {
                    OwnerDocState::ReviewRequested
                        if self.world.live(&doc.facts.latest_session_id) =>
                    {
                        "needs_you"
                    }
                    OwnerDocState::ReviewRequested
                    | OwnerDocState::New
                    | OwnerDocState::Updated => "new",
                    _ => "earlier",
                };
                let doc_at = doc.facts.latest_review_at.as_deref().map(norm).map_or_else(
                    || norm(&doc.summary.published_at),
                    |review| review.max(norm(&doc.summary.published_at)),
                );
                if group_rank(doc_group) < group_rank(group)
                    || (doc_group == group && doc_at >= group_at)
                {
                    group = doc_group;
                    group_at = doc_at;
                    preview = doc_preview(
                        &doc.summary,
                        if doc.summary.state == OwnerDocState::Reviewed {
                            doc.facts.latest_verdict.as_deref()
                        } else {
                            None
                        },
                    );
                }
                open_asks += usize::from(doc_group == "needs_you");
                if group_rank(doc_group) < selected_doc_rank
                    || (group_rank(doc_group) == selected_doc_rank
                        && norm(&doc.summary.published_at) >= selected_doc_at)
                {
                    selected_doc_rank = group_rank(doc_group);
                    selected_doc_at = norm(&doc.summary.published_at);
                    status = doc.summary.state.as_str().to_owned();
                    verdict = if doc.summary.state == OwnerDocState::Reviewed {
                        doc.facts.latest_verdict.clone()
                    } else {
                        None
                    };
                    pr_number = doc.summary.doc.pr_number;
                    author = doc.summary.doc.author_session_name.clone();
                    doc_url = Some(doc_reader_path(&doc.summary));
                }
            }
            let migrated = migrated_items.get(key.as_str()).copied().unwrap_or(0)
                + self
                    .docs
                    .iter()
                    .filter(|doc| doc.key == key)
                    .filter(|doc| {
                        legacy_docs
                            .get(&doc_thread_key(&doc.summary.doc.id))
                            .copied()
                            .unwrap_or(false)
                    })
                    .map(|doc| doc.facts.review_count)
                    .sum::<usize>();
            if marks.done_items.is_none() && migrated > 0 {
                inbox_store(state).migrate_done(&key, migrated)?;
                let entry = marks_map.entry(key.clone()).or_default();
                entry.done_items = Some(i64::try_from(migrated).unwrap_or(i64::MAX));
            }
            let marks = marks_map.get(&key).cloned().unwrap_or_default();
            let folded_by = if marks
                .archived_items
                .is_some_and(|n| usize::try_from(n).ok() == Some(items))
            {
                Some("archived")
            } else if doc_ids.is_empty()
                && senders.iter().all(|id| !self.world.live(id))
                && !matches!(group, "needs_you" | "new")
                && !unread_finished
            {
                Some("ended")
            } else {
                None
            };
            let title = self
                .work
                .title(&key)
                .or_else(|| {
                    self.docs
                        .iter()
                        .find(|doc| doc.key == key)
                        .map(|doc| doc.summary.doc.title.clone())
                })
                .unwrap_or_else(|| {
                    self.world
                        .agent_name(senders.iter().next().map(String::as_str).unwrap_or(""))
                });
            let repo = key
                .split_once(':')
                .and_then(|(_, rest)| rest.rsplit_once('#').map(|(repo, _)| repo.to_owned()))
                .or_else(|| {
                    self.docs
                        .iter()
                        .find(|doc| doc.key == key)
                        .map(|doc| doc.summary.doc.repo.clone())
                })
                .unwrap_or_else(|| {
                    senders
                        .iter()
                        .next()
                        .map_or_else(String::new, |id| self.world.agent_repo(id))
                });
            let kind = if key.starts_with("ticket:") {
                "ticket"
            } else if key.starts_with("pr:") {
                "pr"
            } else if !doc_ids.is_empty() {
                "doc"
            } else {
                "agent"
            };
            let newest_sender = own
                .iter()
                .rev()
                .find_map(|entry| entry.sender_id)
                .map(str::to_owned);
            rows.push(InboxRow {
                thread_key: key.clone(),
                kind,
                title,
                repo,
                status,
                verdict,
                pr_number,
                author,
                group: if folded_by.is_some() { "folded" } else { group },
                preview,
                newest_at,
                message_count: messages,
                revision_count: revisions,
                doc_count: doc_ids.len(),
                open_asks,
                url: thread_path(&key),
                done: is_done(&marks, items),
                session_id: newest_sender,
                doc_id: doc_ids.into_iter().next(),
                doc_url,
                items,
                folded_by,
                agents: senders
                    .into_iter()
                    .map(|id| self.world.agent_name(&id))
                    .collect(),
            });
        }
        Ok(rows)
    }

    fn entries_for(&self, key: &str) -> Vec<ThreadEntry<'_>> {
        self.entries()
            .into_iter()
            .filter(|entry| entry.key == key)
            .collect()
    }

    pub(super) fn message_ids(&self, key: &str) -> BTreeSet<String> {
        self.entries_for(key)
            .into_iter()
            .filter_map(|entry| match entry.item {
                Item::Message(message, _) => Some(message.id.clone()),
                _ => None,
            })
            .collect()
    }

    pub(super) fn canonical_key(&self, key: &str) -> Option<String> {
        let entries = self.entries();
        entries
            .iter()
            .rev()
            .find(|entry| entry.key == key)
            .map(|entry| entry.key.clone())
            .or_else(|| {
                entries
                    .iter()
                    .rev()
                    .find(|entry| entry.legacy_key == key)
                    .map(|entry| entry.key.clone())
            })
    }

    pub(crate) fn doc_key(&self, doc_id: &str) -> Option<(String, String)> {
        self.docs
            .iter()
            .find(|doc| doc.summary.doc.id == doc_id)
            .and_then(|doc| {
                doc.publishes
                    .first()
                    .map(|first| (doc.key.clone(), first.published_at.clone()))
            })
    }

    /// Ask previews a document's portion of a work thread without changing
    /// read state for messages elsewhere in that thread.
    pub(crate) fn doc_items(&self, doc_id: &str) -> Option<Vec<Value>> {
        let (key, first_published_at) = self.doc_key(doc_id)?;
        let first_published_at = norm(&first_published_at);
        let now = OffsetDateTime::now_utc();
        Some(
            self.entries_for(&key)
                .into_iter()
                .filter(|entry| entry.item.at() >= first_published_at)
                .map(|entry| {
                    entry
                        .item
                        .json(&self.world, entry.sender_id.unwrap_or(""), now)
                })
                .collect(),
        )
    }

    fn reply_options(&self, state: &AppState, entries: &[ThreadEntry<'_>]) -> Vec<Value> {
        let mut latest = BTreeMap::<String, String>::new();
        for entry in entries {
            if let Some(sender) = entry.sender_id {
                if !matches!(entry.item, Item::Answered(_)) {
                    latest.insert(sender.to_owned(), entry.item.at());
                }
            }
        }
        let mut ordered: Vec<_> = latest.into_iter().collect();
        ordered.sort_by(|a, b| b.1.cmp(&a.1));
        ordered
            .into_iter()
            .map(|(id, _)| {
                let recipient = super::super::messages::reply_recipient(state, &id);
                let restores = recipient.as_ref().is_some_and(super::super::messages::restores);
                json!({"id": id, "name": self.world.agent_name(&id),
                "status": if self.world.live(&id) { "live" } else { "ended" },
                "can_send": recipient.is_some(), "recipient_id": recipient.as_ref().map(|s| s.id.as_str()),
                "recipient_name": recipient.map(session_display_name), "restores": restores,
                "retired_at": self.world.sessions.get(&id).and_then(|s| s.completed_at.clone())})
            })
            .collect()
    }
}

pub(crate) async fn get_thread(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<ThreadQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    ensure_owner_page_read_allowed(&state, &request)?;
    thread_page(
        &state,
        &key,
        query.at.filter(|_| query.bottom.is_none()),
        web::wants_json(&request),
        web::wants_shell(&state, &request),
    )
}

pub(crate) fn key_for_message(
    state: &AppState,
    message: &OwnerMessage,
) -> Result<String, ApiError> {
    Ok(ThreadCatalog::load(state)?.message_key(message))
}

pub(crate) fn key_for_agent(state: &AppState, session_id: &str) -> Result<String, ApiError> {
    let catalog = ThreadCatalog::load(state)?;
    Ok(catalog
        .entries()
        .into_iter()
        .rev()
        .find(|entry| entry.sender_id == Some(session_id))
        .map_or_else(|| agent_thread_key(session_id), |entry| entry.key))
}

pub(crate) fn path_for_key(key: &str) -> String {
    thread_path(key)
}

pub(crate) fn thread_page(
    state: &AppState,
    key: &str,
    at: Option<String>,
    json_format: bool,
    browser: bool,
) -> Result<Response, ApiError> {
    let catalog = ThreadCatalog::load(state)?;
    let entries = catalog.entries_for(key);
    if entries.is_empty() {
        return Err(ApiError::NotFound("Thread not found"));
    }
    let rows = catalog.rows(state)?;
    let row = rows
        .into_iter()
        .find(|row| row.thread_key == key)
        .ok_or(ApiError::NotFound("Thread not found"))?;
    let store = owner_message_store(state);
    for entry in &entries {
        if let Item::Message(message, _) = &entry.item {
            store.mark_viewed(&message.id)?;
        }
    }
    let follows = entries
        .iter()
        .filter(|entry| matches!(entry.item, Item::Follow(_)))
        .count();
    inbox_store(state).mark_read(key, follows)?;
    let catalog = ThreadCatalog::load(state)?;
    let entries = catalog.entries_for(key);
    let now = OffsetDateTime::now_utc();
    let options = catalog.reply_options(state, &entries);
    let selected = options
        .iter()
        .find(|option| option["status"] == "live" && option["can_send"] == true)
        .or_else(|| options.iter().find(|option| option["can_send"] == true))
        .cloned();
    let reply_to = selected.as_ref().map(|option| {
        json!({"id": option["recipient_id"],
        "name": option["recipient_name"], "restores": option["restores"],
        "retired_at": option["retired_at"]})
    });
    let items: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let sender = entry.sender_id.map(|id| {
                json!({"id": id, "name": catalog.world.agent_name(id),
            "status": if catalog.world.live(id) { "live" } else { "ended" }})
            });
            let mut value = entry
                .item
                .json(&catalog.world, entry.sender_id.unwrap_or(""), now);
            value["sender"] = sender.unwrap_or(Value::Null);
            if let Item::Doc(doc, publish) = &entry.item {
                value["type"] = json!("doc_revision");
                value["doc_id"] = json!(doc.id);
                value["pr"] = json!(doc.pr_number.or(publish.pr_number));
                value["sha"] = json!(publish.commit_sha);
                value["review_state"] = json!(if publish.review_requested {
                    "requested"
                } else {
                    "published"
                });
            }
            value
        })
        .collect();
    let message_ids = catalog.message_ids(key);
    let senders: BTreeSet<_> = entries.iter().filter_map(|entry| entry.sender_id).collect();
    let mut review_actions = Vec::new();
    for sender in &senders {
        review_actions.extend(review_asks(state, sender)?.into_iter().filter(|ask| {
            ask["message_id"]
                .as_str()
                .is_some_and(|id| message_ids.contains(id))
        }));
    }
    if json_format {
        return Ok(Json(
            json!({"thread_key": key, "title": row.title, "status": row.status,
            "repo": row.repo, "can_send": reply_to.is_some(), "reply_to": reply_to,
            "reply_options": options, "items": items, "review_asks": review_actions}),
        )
        .into_response());
    }
    let name = escape_html(&row.title);
    let mut body = format!("<style>{THREAD_STYLE}</style><div class=\"th-head\"><a class=\"dim\" href=\"/inbox\">‹ Inbox</a><span class=\"big\">{name}</span></div><div class=\"items can-quote\">");
    for entry in &entries {
        body.push_str(&render_item(
            &entry.item,
            &catalog.world,
            entry.sender_id.unwrap_or(""),
            now,
        ));
    }
    body.push_str("</div><div class=\"compose\">");
    if let Some(target) = &reply_to {
        if options.len() > 1 {
            body.push_str(
                "<label class=\"hint\" for=\"reply-target\">Reply to <select id=\"reply-target\">",
            );
            for option in &options {
                let id = option["id"].as_str().unwrap_or("");
                body.push_str(&format!(
                    "<option value=\"{}\"{}{}>{}</option>",
                    escape_html(id),
                    if selected
                        .as_ref()
                        .is_some_and(|chosen| chosen["id"] == option["id"])
                    {
                        " selected"
                    } else {
                        ""
                    },
                    if option["can_send"] == false {
                        " disabled"
                    } else {
                        ""
                    },
                    escape_html(option["name"].as_str().unwrap_or(id))
                ));
            }
            body.push_str("</select></label>");
        }
        let hint = if selected
            .as_ref()
            .is_some_and(|option| option["status"] == "ended")
        {
            format!(
                "This agent has ended; a reply goes to {}.",
                escape_html(target["name"].as_str().unwrap_or("agent"))
            )
        } else {
            "Tap a paragraph to quote it.".to_owned()
        };
        body.push_str(&format!("<div class=\"qs\" id=\"qs\"></div><textarea id=\"box\" rows=\"2\" placeholder=\"Write to {}\"></textarea><div class=\"btns\"><button id=\"done\">Done</button><button id=\"archive\">Archive</button><span class=\"msg\" id=\"msg\">{hint}</span><button class=\"send\" id=\"send\">Send</button></div>", escape_html(target["name"].as_str().unwrap_or("agent"))));
    } else {
        body.push_str("<div class=\"btns\"><button id=\"done\">Done</button><button id=\"archive\">Archive</button><span class=\"msg\" id=\"msg\">No agent is left to reply to.</span></div>");
    }
    body.push_str("</div>");
    let config = json!({"page": "thread", "token": docs::issue_doc_token(&state.config, TOKEN_SUBJECT),
        "threadKey": key, "sessionId": selected.as_ref().and_then(|v| v["id"].as_str()),
        "replyOptions": options, "canSend": reply_to.is_some(), "at": at});
    body.push_str(&format!(
        "<script>{INBOX_CLIENT_JS}({});</script>",
        inline_json(&config)
    ));
    body.push_str(&web::reader_injection(browser));
    Ok(html(page_shell_with_status(&row.title, "inbox", "", &body)))
}

fn thread_path(key: &str) -> String {
    let encoded: String = key
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect();
    format!("/inbox/thread/{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(session: &str, kind: &str, number: i64, start: &str, end: Option<&str>) -> WorkClaim {
        WorkClaim {
            id: format!("{session}-{kind}-{number}"),
            repo: "owner/repo".into(),
            number,
            kind: kind.into(),
            session_id: session.into(),
            session_name: None,
            parent_session_id: None,
            source: "explicit".into(),
            worktree_path: None,
            branch: None,
            claimed_at: start.into(),
            ended_at: end.map(str::to_owned),
            end_reason: None,
            ended_by_session_id: None,
            nudged_idle_at: None,
            managed_worktree: false,
            base_sha: None,
            reserved_at: None,
            check_b_due_at: None,
        }
    }

    fn index() -> WorkIndex {
        WorkIndex {
            claims: vec![
                claim(
                    "first",
                    "ticket",
                    1782,
                    "2026-09-01T00:00:00Z",
                    Some("2026-09-02T00:00:00Z"),
                ),
                claim("second", "ticket", 1782, "2026-09-02T00:00:00Z", None),
                claim("reviewer", "pr", 1785, "2026-09-01T00:00:00Z", None),
                claim("first", "pr", 1785, "2026-09-01T00:00:00Z", None),
            ],
            items: BTreeMap::new(),
            links: BTreeMap::new(),
            reader_keys: BTreeMap::new(),
        }
    }

    fn doc(pr: Option<i64>, author: &str) -> crate::owner_docs::OwnerDoc {
        crate::owner_docs::OwnerDoc {
            id: format!("doc-{author}-{pr:?}"),
            repo: "owner/repo".into(),
            path: "docs/memo.html".into(),
            pr_number: pr,
            author_session_id: author.into(),
            author_session_name: None,
            title: "Memo".into(),
            note: None,
            retracted_at: None,
            created_at: "2026-09-01T00:00:00Z".into(),
            updated_at: "2026-09-01T00:00:00Z".into(),
        }
    }

    fn publish(session: &str, pr: Option<i64>, at: &str) -> OwnerDocPublish {
        OwnerDocPublish {
            id: 1,
            doc_id: "doc".into(),
            commit_sha: "abc".into(),
            blob_sha: "def".into(),
            session_id: session.into(),
            review_requested: false,
            published_at: at.into(),
            checkout_root: None,
            review_dismissed_at: None,
            pr_number: pr,
        }
    }

    #[test]
    fn ticket_threads_join_handoff_agents_and_docs_across_prs() {
        let index = index();
        let before = "2026-09-01T12:00:00Z";
        let after = "2026-09-02T12:00:00Z";
        assert_eq!(index.agent_key("first", before), "ticket:owner/repo#1782");
        assert_eq!(index.agent_key("second", after), "ticket:owner/repo#1782");
        assert_eq!(
            index.doc_key(
                &doc(Some(1785), "first"),
                &publish("first", Some(1785), before)
            ),
            "ticket:owner/repo#1782"
        );
        assert_eq!(
            index.doc_key(
                &doc(Some(1802), "second"),
                &publish("second", Some(1802), after)
            ),
            "ticket:owner/repo#1782"
        );
        assert_eq!(index.agent_key("first", after), "pr:owner/repo#1785");
    }

    #[test]
    fn pr_and_docpath_fallbacks_do_not_join_other_work() {
        let mut index = index();
        let at = "2026-09-01T12:00:00Z";
        assert_eq!(index.agent_key("reviewer", at), "pr:owner/repo#1785");
        assert_eq!(
            index.doc_key(
                &doc(Some(1785), "reviewer"),
                &publish("reviewer", Some(1785), at)
            ),
            "pr:owner/repo#1785"
        );
        index.links.insert(("owner/repo".into(), 1785), vec![1782]);
        assert_eq!(
            index.doc_key(
                &doc(Some(1785), "unknown"),
                &publish("unknown", Some(1785), at)
            ),
            "ticket:owner/repo#1782"
        );
        assert_eq!(
            index.doc_key(&doc(None, "unknown"), &publish("unknown", None, at)),
            "docpath:owner/repo/docs/memo.html"
        );
    }

    #[test]
    fn ask_items_start_at_the_docs_first_publication_without_viewing_older_messages() {
        let first = "2026-09-01T12:00:00Z";
        let doc = doc(Some(1785), "first");
        let doc_id = doc.id.clone();
        let message = |id: &str, at: &str| OwnerMessage {
            id: id.into(),
            human: "owner".into(),
            sender_session_id: "first".into(),
            sender_session_name: "First".into(),
            title: id.into(),
            body_markdown: id.into(),
            blocking: false,
            created_at: at.into(),
            first_viewed_at: None,
            handled_at: None,
            handled_via: None,
        };
        let catalog = ThreadCatalog {
            world: World {
                sessions: BTreeMap::new(),
                messages: vec![
                    message("older", "2026-09-01T11:00:00Z"),
                    message("newer", "2026-09-01T13:00:00Z"),
                ],
                replies: Vec::new(),
                notes: Vec::new(),
                follows: Vec::new(),
                finished: Vec::new(),
                agent_replies: Vec::new(),
                marks: BTreeMap::new(),
            },
            work: index(),
            docs: vec![DocData {
                summary: crate::owner_docs::OwnerDocSummary {
                    doc,
                    state: crate::owner_docs::OwnerDocState::New,
                    latest_commit_sha: "abc".into(),
                    latest_blob_sha: "def".into(),
                    published_at: first.into(),
                    publish_count: 1,
                    review_undelivered: false,
                },
                facts: DocInboxFacts::default(),
                publishes: vec![publish("first", Some(1785), first)],
                key: "ticket:owner/repo#1782".into(),
            }],
            ask_answer_keys: BTreeMap::new(),
        };
        let items = catalog.doc_items(&doc_id).unwrap();
        assert_eq!(items.len(), 2); // The publication and the newer message.
        assert!(items.iter().any(|item| item["id"] == "newer"));
        assert!(!items.iter().any(|item| item["id"] == "older"));
        assert!(catalog
            .world
            .messages
            .iter()
            .all(|message| message.first_viewed_at.is_none()));
    }

    #[tokio::test]
    async fn thread_url_keeps_the_full_work_key() {
        use tower::ServiceExt;
        let app = axum::Router::new().route(
            "/inbox/thread/{key}",
            axum::routing::get(|Path(key): Path<String>| async move { key }),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri(thread_path("ticket:owner/repo#1782"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ticket:owner/repo#1782");
    }
}

pub(super) async fn mark_done(state: &AppState, key: &str) -> Result<usize, ApiError> {
    let catalog = ThreadCatalog::load(state)?;
    let entries = catalog.entries_for(key);
    if entries.is_empty() {
        return Err(ApiError::NotFound("Thread not found"));
    }
    let replied = catalog.world.replied();
    let messages = owner_message_store(state);
    let mut finished_rows = BTreeSet::new();
    let mut docs = BTreeSet::new();
    for entry in &entries {
        match &entry.item {
            Item::Message(message, _) => {
                messages.mark_viewed(&message.id)?;
                if message.blocking
                    && message.handled_at.is_none()
                    && !replied.contains(message.id.as_str())
                {
                    messages.mark_handled_via(&message.id, "inbox")?;
                }
            }
            Item::Turn(row) => {
                finished_rows.insert((row.session_id.as_str(), row.completed_at.as_str()));
            }
            Item::Doc(doc, _) => {
                docs.insert(doc.id.as_str());
            }
            _ => {}
        }
    }
    for (sender, completed_at) in finished_rows {
        if let Some(turns) = state.session_store.turn_message_store() {
            turns.mark_finished_read(sender, completed_at, OffsetDateTime::now_utc())?;
        }
    }
    let store = owner_doc_store(state);
    for doc_id in docs {
        if store
            .summary(doc_id)?
            .is_some_and(|summary| summary.state == OwnerDocState::ReviewRequested)
        {
            store.dismiss_review(doc_id)?;
        }
    }
    let row = ThreadCatalog::load(state)?
        .rows(state)?
        .into_iter()
        .find(|row| row.thread_key == key)
        .ok_or(ApiError::NotFound("Thread not found"))?;
    inbox_store(state).mark_done(key, row.items)?;
    Ok(row.items)
}

pub(crate) async fn post_archive(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    ensure_inbox_write_allowed(&state, &headers, peer_addr, "/inbox/archive")?;
    let payload: DoneRequest = docs::parse_json_body(&body)?;
    let key = payload.thread_key.trim();
    let items = mark_done(&state, key).await?;
    inbox_store(&state).mark_archive(key, items)?;
    Ok(Json(
        json!({"thread_key": key, "archived": true, "done": true}),
    ))
}

pub(crate) async fn post_unarchive(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    ensure_inbox_write_allowed(&state, &headers, peer_addr, "/inbox/unarchive")?;
    let payload: DoneRequest = docs::parse_json_body(&body)?;
    let key = payload.thread_key.trim();
    if !ThreadCatalog::load(&state)?
        .rows(&state)?
        .iter()
        .any(|row| row.thread_key == key)
    {
        return Err(ApiError::NotFound("Thread not found"));
    }
    inbox_store(&state).unarchive(key)?;
    Ok(Json(json!({"thread_key": key, "archived": false})))
}

#[derive(Deserialize)]
struct ThreadSendRequest {
    #[serde(default)]
    to: Option<String>,
}

pub(crate) async fn post_thread_send(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    ensure_inbox_write_allowed(
        &state,
        &headers,
        peer_addr,
        &format!("{}/send", thread_path(&key)),
    )?;
    let selector: ThreadSendRequest = docs::parse_json_body(&body)?;
    let catalog = ThreadCatalog::load(&state)?;
    let entries = catalog.entries_for(&key);
    if entries.is_empty() {
        return Err(ApiError::NotFound("Thread not found"));
    }
    let options = catalog.reply_options(&state, &entries);
    let chosen = match selector.to.as_deref() {
        Some(id) => options
            .iter()
            .find(|option| option["id"] == id && option["can_send"] == true),
        None => options
            .iter()
            .find(|option| option["status"] == "live" && option["can_send"] == true)
            .or_else(|| options.iter().find(|option| option["can_send"] == true)),
    }
    .ok_or_else(|| conflict(NO_RECIPIENT))?;
    let id = chosen["id"]
        .as_str()
        .ok_or_else(|| conflict(NO_RECIPIENT))?;
    let payload: SendRequest = docs::parse_json_body(&body)?;
    send_to_agent(&state, id, payload, Some(&key)).await
}
