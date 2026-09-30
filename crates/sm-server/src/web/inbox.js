import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, config, bus, usePoll, openPanel, closePanel, registerPanel, Seg, toast } from './ui.js';
import { Reader } from './reader.js';
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
const write = (path, body) => api(path, { method: 'POST', headers: { 'X-SM-Doc-Token': config.inbox_token }, body });

export function InboxPage({ openRef }) {
  const [filter, setFilter] = useState('open');
  const [data, error, reload] = usePoll(() => api(`/inbox?format=json&filter=${filter}`), 30000, [filter]);
  const rowRef = row => row.kind === 'doc' ? `doc:${row.url}` : `thread:${row.session_id}`;
  const selected = data?.rows.find(row => rowRef(row) === openRef);
  const [busy, setBusy] = useState(false);
  const done = async () => {
    if (!selected || busy) return;
    setBusy(true);
    try { await write('/inbox/done', { thread_key: selected.thread_key }); closePanel(); reload(); }
    catch (e) { toast(e.message); }
    finally { setBusy(false); }
  };
  useEffect(() => {
    const key = e => {
      if (e.key === 'e' && !e.metaKey && !e.ctrlKey && !e.altKey && !e.target.closest('input,textarea,select,[contenteditable]')) { e.preventDefault(); done(); }
    };
    document.addEventListener('keydown', key);
    const off = bus.on('inbox-done', done);
    return () => { document.removeEventListener('keydown', key); off(); };
  }, [selected, busy]);
  const inline = openRef?.startsWith('doc:') || openRef?.startsWith('thread:');
  return html`<div class=${`inbox-layout ${inline ? 'reading' : ''}`}>
    <section class="inbox-list" aria-label="Inbox threads">
      <div class="inbox-filters"><${Seg} label="Inbox filter" value=${filter} onChange=${setFilter}
        options=${['open','docs','done'].map(value => ({value, label: value[0].toUpperCase()+value.slice(1)}))} /></div>
      ${error ? html`<p role="alert">${error.message}</p>` : null}
      ${!data ? html`<p class="empty">Loading…</p>` : data.rows.length === 0 ? html`<p class="empty">Nothing here.</p>` : null}
      ${(filter === 'open' ? ['needs_you','new','earlier'] : ['all']).map(group => {
        const rows = (data?.rows || []).filter(r => group === 'all' || r.group === group);
        return rows.length ? html`<div>${group !== 'all' ? html`<h2 class=${group === 'needs_you' ? 'magenta' : ''}>${({needs_you:'Needs you',new:'New',earlier:'Earlier'})[group]}</h2>` : null}
          ${rows.map(row => html`<button class=${`inbox-row ${rowRef(row) === openRef ? 'selected' : ''}`} onClick=${() => openPanel(rowRef(row))}>
            <strong>${row.title}</strong><span>${row.preview}</span><small>${row.repo} · ${row.status}${row.pr_number ? ` · #${row.pr_number}` : ''}</small>
          </button>`)}</div>` : null;
      })}
    </section>
    <section class="inbox-reader">
      ${inline ? html`<div class="inbox-actions"><button class="btn sm" onClick=${closePanel}>← Inbox</button><span></span>
        ${selected ? html`<button class="btn sm" disabled=${busy} onClick=${done}>Done <kbd>e</kbd></button>` : null}</div>` : null}
      ${openRef?.startsWith('doc:') ? html`<${Reader} key=${openRef} id=${openRef.slice(4)} />`
        : openRef?.startsWith('thread:') ? html`<${Thread} key=${openRef} id=${openRef.slice(7)} />`
        : html`<div class="empty">Choose a thread or document to read.</div>`}
    </section>
  </div>`;
}

export function Thread({ id, controls }) {
  const [data, error, reload] = usePoll(() => api(`/inbox/agent/${encodeURIComponent(id)}?format=json`), 30000, [id]);
  const [body, setBody] = useState('');
  const [quotes, setQuotes] = useState([]);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState('');
  const attempt = useRef(null);
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
    const payload = { body, quotes };
    const signature = JSON.stringify(payload);
    if (attempt.current?.signature !== signature) attempt.current = {signature, id: crypto.randomUUID()};
    setBusy(true); setFailure('');
    try {
      await write(`/inbox/agent/${encodeURIComponent(id)}/send`, {...payload, submission_id: attempt.current.id});
      attempt.current = null; setBody(''); setQuotes([]); reload();
    } catch (e) { setFailure(e.message); }
    finally { setBusy(false); }
  };
  return html`<section class="thread-reader">
    <div class="reader-bar"><strong class="reader-title">${data?.title || 'Thread'}</strong><span>${data?.status}</span>${controls}</div>
    ${error ? html`<p role="alert">${error.message}</p>` : null}
    <div class="thread-items" onClick=${quote}>${data?.items.map((item,i) => html`<div key=${i} dangerouslySetInnerHTML=${{__html:safeThreadHtml(item.html)}} />`)}${!data ? 'Loading…' : null}</div>
    <div class="thread-compose">
      ${quotes.map((q,i) => html`<blockquote>${q.quote}<button class="icon-btn" disabled=${busy} title="Remove quote" onClick=${() => setQuotes(quotes.filter((_,n) => n !== i))}>×</button></blockquote>`)}
      ${data?.can_send ? html`<textarea aria-label="Reply" placeholder=${`Write to ${data.reply_to}… Click a paragraph to quote it.`} value=${body} disabled=${busy} onInput=${e => setBody(e.target.value)} />
        <button class="btn pri" disabled=${busy || (!body.trim() && !quotes.length)} onClick=${send}>${busy ? 'Sending…' : 'Send'}</button>` : html`<p>No agent is left to reply to.</p>`}
      ${failure ? html`<p role="alert">${failure}</p>` : null}
    </div>
  </section>`;
}
registerPanel('thread', Thread);
