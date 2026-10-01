import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks';
import { html, api, config, bus, usePoll, openPanel, closePanel, registerPanel, navigate, Seg, toast, typingIn, Links, age, stored, store } from './ui.js';
import { Reader, readerPath } from './reader.js';
export function safeThreadHtml(source) {
  const doc = new DOMParser().parseFromString(source, 'text/html');
  const tags = new Set('DIV SPAN P H1 H2 H3 H4 H5 H6 BLOCKQUOTE PRE CODE STRONG EM DEL UL OL LI TABLE THEAD TBODY TR TH TD A BR HR IMG SUP SUB INPUT'.split(' '));
  for (const node of [...doc.body.querySelectorAll('*')]) {
    if (!tags.has(node.tagName)) { node.remove(); continue; }
    for (const attr of [...node.attributes]) {
      if (!['class','id','data-msg','data-sm-line','href','src','alt','title','type','checked','disabled','start'].includes(attr.name)) node.removeAttribute(attr.name);
    }
    for (const attr of ['href','src']) {
      if (node.hasAttribute(attr)) {
        try { if (!['http:','https:'].includes(new URL(node.getAttribute(attr),location.origin).protocol)) node.removeAttribute(attr); }
        catch (_) { node.removeAttribute(attr); }
      }
    }
    if (node.tagName === 'INPUT') { node.type = 'checkbox'; node.disabled = true; }
  }
  return doc.body.innerHTML;
}
// The thread's agent as `/watch/state` shows it: claims, jobs and facts.
function useWatchedAgent(id) {
  const [doc, , reload] = usePoll(() => id ? api(`/watch/state?session=${encodeURIComponent(id)}`) : Promise.resolve(null), 30000, [id]);
  return [doc?.sessions?.find(session => session.id === id) || null, reload];
}
const factText = facts => !facts ? '' : facts.agent?.state === 'stopped' ? 'Stopped'
  : `${facts.agent?.state === 'working' ? '● Working' : '○ Idle'}${facts.agent?.since ? ` ${age(facts.agent.since)}` : ''}`;
const write = (path, body) => api(path, { method: 'POST', headers: { 'X-SM-Doc-Token': config.inbox_token }, body });
const docRevision = item => {
  const link = new DOMParser().parseFromString(item.html, 'text/html').querySelector('a[href]');
  return link ? { path: readerPath(link.getAttribute('href')), title: link.textContent.trim() } : null;
};
const replyTime = value => value ? new Date(value).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' }) : 'its last turn';

export function InboxPage({ openRef }) {
  const [filter, setFilter] = useState('open');
  const [foldOpen, setFoldOpen] = useState(() => stored('sm-inbox-folded-open', false));
  const [data, error, reload] = usePoll(() => api(`/inbox?format=json&filter=${filter}`), 30000, [filter]);
  const rowRef = row => `work:${row.thread_key}`;
  const selected = data?.rows.find(row => rowRef(row) === openRef);
  const [busy, setBusy] = useState(false);
  const workKey = openRef?.startsWith('work:') ? openRef.slice(5) : null;
  const threadId = openRef?.startsWith('thread:') ? openRef.slice(7) : selected?.session_id;
  const [agent, reloadAgent] = useWatchedAgent(threadId);
  const [retiring, setRetiring] = useState(false);
  useEffect(() => setRetiring(false), [threadId]);
  const retire = async () => {
    if (!agent || busy) return;
    setBusy(true);
    try { await api(`/sessions/${encodeURIComponent(agent.id)}/retire`, { method: 'POST', body: {} }); toast(`Retired ${agent.name}`); setRetiring(false); reloadAgent(); reload(); }
    catch (e) { toast(e.message); }
    finally { setBusy(false); }
  };
  const answered = async () => {
    if (!threadId || busy) return;
    setBusy(true);
    try { await api(`/sessions/${encodeURIComponent(threadId)}/needs-you/answered`, { method: 'POST', body: {} }); toast('Marked answered'); reloadAgent(); reload(); }
    catch (e) { toast(e.message); }
    finally { setBusy(false); }
  };
  const done = async () => {
    if (!selected || busy) return;
    setBusy(true);
    try { await write('/inbox/done', { thread_key: selected.thread_key }); closePanel(); reload(); }
    catch (e) { toast(e.message); }
    finally { setBusy(false); }
  };
  const archive = async (row = selected) => {
    if (!row || busy) return;
    setBusy(true);
    try {
      const unarchive = row.folded_by === 'archived';
      await write(`/inbox/${unarchive ? 'unarchive' : 'archive'}`, { thread_key: row.thread_key });
      if (row.thread_key === workKey) closePanel();
      reload();
    } catch (e) { toast(e.message); }
    finally { setBusy(false); }
  };
  useEffect(() => {
    const key = e => {
      if (e.key === 'e' && !e.metaKey && !e.ctrlKey && !e.altKey && !typingIn(e)) { e.preventDefault(); done(); }
      if (e.key === 'y' && !e.metaKey && !e.ctrlKey && !e.altKey && !typingIn(e) && selected) { e.preventDefault(); archive(); }
    };
    document.addEventListener('keydown', key);
    const off = bus.on('inbox-done', done);
    return () => { document.removeEventListener('keydown', key); off(); };
  }, [selected, busy]);
  const row = item => html`<div class="inbox-entry" key=${item.thread_key}>
    <button class=${`inbox-row ${rowRef(item) === openRef ? 'selected' : ''}`} onClick=${() => openPanel(rowRef(item))}>
      <strong>${item.title}</strong><span>${item.preview}</span>
      <small>${[...item.agents, ...(item.doc_count ? [`${item.doc_count} docs, ${item.revision_count} revisions`] : [])].join(' · ') || item.repo}</small>
    </button>
    <button class="inbox-entry-action" disabled=${busy} onClick=${() => archive(item)}>${item.folded_by === 'archived' ? 'Unarchive' : 'Archive'}</button>
  </div>`;
  const folded = (data?.rows || []).filter(item => item.group === 'folded');
  const foldNames = folded.slice(0, 3).map(item => item.title).join(', ');
  const inline = openRef?.startsWith('doc:') || openRef?.startsWith('thread:') || !!workKey;
  return html`<div class=${`inbox-layout ${inline ? 'reading' : ''}`}>
    <section class="inbox-list" aria-label="Inbox threads">
      <div class="inbox-filters"><${Seg} label="Inbox filter" value=${filter} onChange=${setFilter}
        options=${['open','docs','done'].map(value => ({value, label: value[0].toUpperCase()+value.slice(1)}))} /></div>
      ${error ? html`<p role="alert">${error.message}</p>` : null}
      ${!data ? html`<p class="empty">Loading…</p>` : data.rows.length === 0 ? html`<p class="empty">Nothing here.</p>` : null}
      ${(filter === 'open' ? ['needs_you','finished','new','earlier'] : ['all']).map(group => {
        const rows = (data?.rows || []).filter(r => group === 'all' || r.group === group);
        return rows.length ? html`<div>${group !== 'all' ? html`<h2 class=${({needs_you:'magenta',finished:'cyan'})[group] || ''}>${({needs_you:'Needs you',finished:'Finished',new:'New',earlier:'Earlier',folded:'Folded'})[group]}</h2>` : null}
          ${rows.map(row)}</div>` : null;
      })}
      ${filter === 'open' && folded.length ? html`<div class="inbox-fold">
        <button class="inbox-fold-toggle" aria-expanded=${foldOpen} onClick=${() => { store('sm-inbox-folded-open', !foldOpen); setFoldOpen(!foldOpen); }}>
          ${foldOpen ? '▾' : '▸'} Folded · ${folded.length} threads (${foldNames}${folded.length > 3 ? ', …' : ''})
        </button>${foldOpen ? folded.map(row) : null}
      </div>` : null}
    </section>
    <section class="inbox-reader">
      ${inline ? html`<div class="inbox-actions"><button class="btn sm" onClick=${closePanel}>← Inbox</button><span></span>
        ${agent?.facts?.you?.dismissible ? html`<button class="btn sm" disabled=${busy} title="You answered elsewhere; clear the question" onClick=${answered}>✓ Answered</button>` : null}
        ${selected ? html`<button class="btn sm" disabled=${busy} onClick=${done}>Done <kbd>e</kbd></button>` : null}
        ${selected ? html`<button class="btn sm" disabled=${busy} onClick=${() => archive()}>${selected.folded_by === 'archived' ? 'Unarchive' : 'Archive'} <kbd>y</kbd></button>` : null}
        ${agent && agent.state !== 'stopped' ? retiring
          ? html`<span class="confirm">Retire ${agent.name}?
              <button class="btn sm danger" disabled=${busy} onClick=${retire}>Retire</button>
              <button class="btn sm" onClick=${() => setRetiring(false)}>Cancel</button></span>`
          : html`<button class="btn sm" onClick=${() => setRetiring(true)}>Retire…</button>` : null}</div>` : null}
      ${openRef?.startsWith('doc:') ? html`<${Reader} key=${openRef} id=${openRef.slice(4)} />`
        : workKey || threadId ? html`<${Thread} key=${openRef} id=${threadId} workKey=${workKey} agent=${agent} />`
        : html`<div class="empty">Choose a thread or document to read.</div>`}
    </section>
  </div>`;
}

export function Thread({ id, workKey, controls, agent }) {
  const endpoint = workKey ? `/inbox/thread/${encodeURIComponent(workKey)}` : `/inbox/agent/${encodeURIComponent(id)}`;
  const [data, error, reload] = usePoll(() => api(`${endpoint}?format=json`), 30000, [endpoint]);
  const [body, setBody] = useState('');
  const [quotes, setQuotes] = useState([]);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState('');
  const [targetId, setTargetId] = useState(null);
  const attempt = useRef(null);
  const items = useRef(null);
  const scrollOnLoad = useRef(true);
  const pinned = useRef(false);
  const options = data?.reply_options || [];
  const defaultTarget = options.find(option => option.status === 'live' && option.can_send)
    || options.find(option => option.can_send);
  const target = options.find(option => option.id === targetId && option.can_send) || defaultTarget;
  const replyTo = target ? { name: target.recipient_name || target.name, restores: target.restores, retired_at: target.retired_at } : data?.reply_to;
  useLayoutEffect(() => {
    if (data && scrollOnLoad.current && items.current) {
      const at = new URLSearchParams(location.search).get('at');
      const target = at && [...items.current.querySelectorAll('[id]')].find(node => node.id === at);
      if (target) {
        target.scrollIntoView({block:'center'});
        target.classList.add('thread-highlight');
        setTimeout(() => target.classList.remove('thread-highlight'), 2000);
      } else { items.current.scrollTop = items.current.scrollHeight; pinned.current = true; }
      scrollOnLoad.current = false;
    }
  }, [data]);
  // Keep the newest message in view when the links row arrives above the
  // thread while the reader is at the bottom.
  useEffect(() => {
    const el = items.current;
    const watch = () => { pinned.current = Math.abs(el.scrollHeight - el.clientHeight - el.scrollTop) < 2; };
    el?.addEventListener('scroll', watch);
    return () => el?.removeEventListener('scroll', watch);
  }, []);
  useLayoutEffect(() => {
    if (pinned.current && items.current) items.current.scrollTop = items.current.scrollHeight;
  }, [!!agent]);
  const quote = e => {
    const link = e.target.closest('a');
    if (link) {
      const url = new URL(link.href);
      if (url.origin === location.origin && url.pathname.startsWith('/docs/')) { e.preventDefault(); openPanel(`doc:${url.pathname}${url.search}`); }
      const match = url.href.match(/^https:\/\/github.com\/([^/]+\/[^/]+)\/(?:issues|pull)\/(\d+)/);
      if (match) { e.preventDefault(); openPanel(`ticket:${match[1]}#${match[2]}`); }
      return;
    }
    if (!data?.can_send || busy) return;
    const paragraph = e.target.closest('[data-sm-line]');
    const message = paragraph?.closest('[data-msg]');
    if (message) {
      const q = { message_id: message.dataset.msg, quote: paragraph.textContent.trim() };
      setQuotes(previous => previous.some(x => x.message_id === q.message_id && x.quote === q.quote) ? previous : [...previous, q]);
    }
  };
  const send = async () => {
    if (busy || (!body.trim() && !quotes.length)) return;
    const payload = { body, quotes, ...(target ? { to: target.id } : {}) };
    const signature = JSON.stringify(payload);
    if (attempt.current?.signature !== signature) attempt.current = {signature, id: crypto.randomUUID()};
    setBusy(true); setFailure('');
    try {
      await write(`${endpoint}/send`, {...payload, submission_id: attempt.current.id});
      scrollOnLoad.current = true;
      attempt.current = null; setBody(''); setQuotes([]); reload();
    } catch (e) { setFailure(e.message); }
    finally { setBusy(false); }
  };
  return html`<section class="thread-reader">
    <div class="reader-bar"><strong class="reader-title">${data?.title || 'Thread'}</strong><span>${data?.status}</span>${controls}</div>
    ${agent ? html`<div class="thread-links"><${Links} ticket=${(agent.claims || []).find(item => item.kind === 'ticket')}
      prs=${(agent.claims || []).filter(item => item.kind === 'pr')}
      agent=${{ id: agent.id, name: agent.name, provider: agent.provider, fact: factText(agent.facts) }}
      jobs=${agent.jobs || []} /></div>` : null}
    ${error ? html`<p role="alert">${error.message}</p>` : null}
    <div class="thread-items" ref=${items} onClick=${quote}>${data?.items.map((item,i) => {
      if (item.type === 'doc_revision') {
        const doc = docRevision(item);
        return html`<div key=${i} class="thread-doc-card">
          <span class="thread-sender">${item.sender?.name || 'Document'} · ${new Date(item.at).toLocaleString()}</span>
          <strong>${doc?.title || 'Document revision'}</strong>
          <small>${item.pr ? `PR #${item.pr} · ` : ''}${item.sha?.slice(0, 7) || ''} · ${item.review_state === 'requested' ? 'Review requested' : 'Published'}</small>
          ${doc?.path ? html`<span class="thread-doc-actions"><button class="btn sm" onClick=${() => openPanel(`doc:${doc.path}`)}>Open</button><button class="btn sm" onClick=${() => openPanel(`doc:${doc.path}#sm-review`)}>Review</button></span>` : null}
        </div>`;
      }
      const sender = item.sender?.name && !/class="[^"]*\bme\b/.test(item.html);
      return html`<div key=${i} class="thread-entry">
        ${sender ? html`<span class="thread-sender">${item.sender.name}</span>` : null}
        ${item.type === 'turn'
          ? html`<div class="b turn"><div class="lbl">${item.finished === false ? 'Reply' : 'Last turn'} · ${new Date(item.at).toLocaleTimeString([], {hour:'numeric', minute:'2-digit'})}</div><div class="md" dangerouslySetInnerHTML=${{__html:safeThreadHtml(item.html)}} /></div>`
          : html`<div dangerouslySetInnerHTML=${{__html:safeThreadHtml(item.html)}} />`}
      </div>`;
    })}${!data ? 'Loading…' : null}</div>
    ${(data?.review_asks || []).map(ask => html`<${ReviewAsk} key=${ask.request_id} ask=${ask} onDone=${reload} />`)}
    <div class="thread-compose">
      ${quotes.map((q,i) => html`<blockquote>${q.quote}<button class="icon-btn" disabled=${busy} title="Remove quote" onClick=${() => setQuotes(quotes.filter((_,n) => n !== i))}>×</button></blockquote>`)}
      ${data?.can_send ? html`${options.length > 1 ? html`<label class="thread-target">Reply to <select aria-label="Reply to" value=${target?.id} disabled=${busy} onChange=${e => setTargetId(e.target.value)}>
          ${options.map(option => html`<option value=${option.id} disabled=${!option.can_send}>${option.name}${option.restores ? ' · restores' : option.status === 'ended' ? ' · ended' : ''}</option>`)}</select></label>` : null}
        ${replyTo?.restores ? html`<p class="thread-reply-hint">${target?.name || replyTo.name} retired at ${replyTime(replyTo.retired_at)}; replying brings it back.</p>` : null}
        <textarea aria-label="Reply" placeholder=${`Write to ${replyTo?.name || 'agent'}… Click a paragraph to quote it.`} value=${body} disabled=${busy} onInput=${e => setBody(e.target.value)} />
        <button class="btn pri" disabled=${busy || (!body.trim() && !quotes.length)} onClick=${send}>${busy ? 'Sending…' : 'Send'}</button>` : html`<p>No agent is left to reply to.</p>`}
      ${failure ? html`<p role="alert">${failure}</p>` : null}
    </div>
  </section>`;
}
/** "PR #n has no reviewer" (1768 G6): run the policy again, change it, or review it yourself. */
function ReviewAsk({ ask, onDone }) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState('');
  const act = async (path, text) => {
    setBusy(true); setFailure('');
    try {
      const result = await api(`/client/review-requests/${encodeURIComponent(ask.request_id)}/${path}`, { method: 'POST', body: {} });
      toast(result.state === 'no_reviewer' ? `Still no reviewer for PR #${ask.pr_number}` : text);
      onDone();
    } catch (e) { setFailure(e.message); } finally { setBusy(false); }
  };
  return html`<div class="review-ask"><strong class="magenta">PR #${ask.pr_number} has no reviewer</strong>
    <div class="row"><button class="btn sm pri" disabled=${busy} onClick=${() => act('retry', `Review requested again for PR #${ask.pr_number}`)}>Retry now</button>
      <button class="btn sm" disabled=${busy} onClick=${() => navigate('/settings#reviews')}>Change policy</button>
      <button class="btn sm" disabled=${busy} onClick=${() => act('owner', `You review PR #${ask.pr_number}; sm wakes the author when your review lands`)}>Review it myself</button></div>
    ${failure ? html`<p role="alert" class="err">${failure}</p>` : null}</div>`;
}
// In another page's reading pane the thread fetches its own agent.
function ThreadPanel({ id, controls }) {
  const [agent] = useWatchedAgent(id);
  return html`<${Thread} id=${id} controls=${controls} agent=${agent} />`;
}
registerPanel('thread', ThreadPanel);
