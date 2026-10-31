// Notes page and pane (1821 B3). The same view renders at both widths.
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, api, bus, toast, navigate, openPanel, stored, store, clientNav } from './ui.js';
import { EFFORTS } from './start.js';
import { ActivitySummary, NoteActivity, useActivityPreviews, recordNoteActivity } from './notes-activity.js';

const notePath = id => `/notes/${encodeURIComponent(id)}`;
const firstLine = body => (body.trim().split('\n')[0] || '').replace(/^#+\s*/, '').slice(0, 80);
const issueParts = body => {
  const [line, ...rest] = body.trim().split('\n');
  return { title: (line || '').replace(/^#+\s*/, '').trim(), body: rest.join('\n').trim() };
};
const age = at => {
  const seconds = Math.max(0, (Date.now() - Date.parse(at)) / 1000);
  if (seconds < 60) return 'now';
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
};
const selectedText = (editor, body) => editor?.selectionStart !== editor?.selectionEnd
  ? editor.value.slice(editor.selectionStart, editor.selectionEnd) : body;

function Snippet({ hit }) {
  const bytes = new TextEncoder().encode(hit.snippet);
  const ranges = (hit.matches || []).filter(m => m.start >= 0 && m.end <= bytes.length && m.end > m.start);
  if (!ranges.length) return html`<span>${hit.snippet}</span>`;
  const decoder = new TextDecoder();
  const parts = []; let at = 0;
  for (const match of ranges) {
    if (match.start < at) continue;
    parts.push(decoder.decode(bytes.slice(at, match.start)));
    parts.push(html`<mark>${decoder.decode(bytes.slice(match.start, match.end))}</mark>`);
    at = match.end;
  }
  parts.push(decoder.decode(bytes.slice(at)));
  return parts;
}

function NoteActions({ noteId, text, summary, canType, onType, target, repos }) {
  const [popover, setPopover] = useState(null);
  const [pending, setPending] = useState(null);
  const [agent, setAgent] = useState(() => stored('sm-notes-last-agent', { provider: 'claude', model: '', effort: '' }));
  const [repo, setRepo] = useState(() => stored('sm-notes-last-repo', ''));
  const [models, setModels] = useState([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [position, setPosition] = useState({ top: 0, left: 0 });
  useEffect(() => {
    if (popover !== 'agent') return;
    let active = true;
    api(`/client/session-models?provider=${encodeURIComponent(agent.provider)}`)
      .then(value => { if (active) setModels(value.models || []); })
      .catch(() => { if (active) setModels([]); });
    return () => { active = false; };
  }, [popover, agent.provider]);
  const copy = async e => { e.stopPropagation(); try { await navigator.clipboard.writeText(await text()); toast('Copied note'); } catch (err) { setError(err.message); } };
  const remember = async (kind, destination, label, content) => {
    const event = { id: crypto.randomUUID(), kind, target: destination, label, body: content };
    try { await recordNoteActivity(noteId, event); }
    catch (_) { setPending(event); setError('Done, but activity wasn’t saved.'); }
  };
  const retryActivity = async () => {
    setBusy(true);
    try { await recordNoteActivity(noteId, pending); setPending(null); setError(''); }
    catch (_) { setError('Activity still couldn’t be saved. Your action is already complete.'); }
    finally { setBusy(false); }
  };
  const typeText = async e => {
    e.stopPropagation();
    if (busy || pending) return;
    setBusy(true); setError('');
    try {
      const content = await text();
      if (!content.trim()) throw new Error('Write some text first.');
      if (!await onType(content)) throw new Error('The terminal was not connected or not shown, so the text may be missing or incomplete.');
      toast(`Added to ${target.label} · press Enter in the terminal to send`);
      await remember('terminal_added', target.id, target.label, content);
    } catch (err) { setError(err.message); }
    finally { setBusy(false); }
  };
  const run = async e => {
    e?.preventDefault(); e?.stopPropagation();
    if (busy || pending) return;
    setBusy(true); setError('');
    try {
      const content = await text();
      if (!content.trim()) throw new Error('Write some text first.');
      if (popover === 'agent') {
        const settings = await api('/client/settings');
        const workspace = settings.new_agent.workspaces?.[0];
        if (!workspace) throw new Error('Set a default workspace in Settings first.');
        const body = { provider: agent.provider, working_dir: workspace, initial_message: content };
        if (agent.model) body.model = agent.model;
        if (agent.effort) body.reasoning_effort = agent.effort;
        const started = await api('/client/sessions', { method: 'POST', body });
        store('sm-notes-last-agent', agent);
        toast(`Started ${started.name}`, () => openPanel(`agent:${started.id}`));
        await remember('agent_started', started.id, started.name, content);
      } else {
        const issue = issueParts(content);
        if (!issue.title) throw new Error('The first line must contain a title.');
        const created = await api('/github/issues', { method: 'POST', body: { repo, ...issue } });
        bus.emit('board-changed');
        store('sm-notes-last-repo', repo);
        toast(`Filed #${created.number}${created.board_note ? ' · Board update pending' : ''}`, () => window.open(created.url, '_blank', 'noopener'));
        const canonicalRepo = new URL(created.url).pathname.split('/').slice(1, 3).join('/');
        await remember('ticket', `${canonicalRepo}#${created.number}`, issue.title, content);
      }
      setPopover(null);
    } catch (err) { setError(err.message); }
    finally { setBusy(false); }
  };
  const toggle = (kind, e) => {
    e.stopPropagation(); setError('');
    const rect = e.currentTarget.getBoundingClientRect();
    setPosition({ top: Math.min(rect.bottom + 4, window.innerHeight - 260), left: Math.min(rect.left, window.innerWidth - 320) });
    setPopover(popover === kind ? null : kind);
  };
  return html`<div class="notes-actions" onClick=${e => e.stopPropagation()}>
    <button type="button" onClick=${copy}>Copy</button>
    ${canType ? html`<button type="button" disabled=${busy || !!pending} onClick=${typeText}>Type into terminal</button>` : null}
    <span class="notes-action-wrap"><button type="button" aria-expanded=${popover === 'agent'} disabled=${busy || !!pending} onClick=${e => toggle('agent', e)}>Start agent ▾</button>
      ${popover === 'agent' ? html`<form class="notes-popover" style=${`top:${Math.max(8, position.top)}px;left:${Math.max(8, position.left)}px`} onSubmit=${run}>
        <strong>${firstLine(summary) || summary.slice(0, 80) || 'Empty note'}</strong>
        <label>Provider<select value=${agent.provider} onChange=${e => setAgent({ provider: e.target.value, model: '', effort: '' })}>
          <option value="claude">Claude</option><option value="codex-fork">Codex</option></select></label>
        <label>Model<select value=${agent.model || ''} onChange=${e => setAgent({ ...agent, model: e.target.value })}>
          <option value="">Provider default</option>${models.map(model => html`<option value=${model}>${model}</option>`)}</select></label>
        <label>Effort<select value=${agent.effort || ''} onChange=${e => setAgent({ ...agent, effort: e.target.value })}>
          <option value="">Default</option>${(EFFORTS[agent.provider] || []).map(value => html`<option value=${value}>${value}</option>`)}</select></label>
        ${error ? html`<span class="err">${error}</span>` : null}
        <button class="btn pri" disabled=${busy} type="submit">${busy ? 'Starting…' : 'Start'}</button>
      </form>` : null}</span>
    <span class="notes-action-wrap"><button type="button" aria-expanded=${popover === 'ticket'} disabled=${busy || !!pending} onClick=${e => toggle('ticket', e)}>File ticket ▾</button>
      ${popover === 'ticket' ? html`<form class="notes-popover" style=${`top:${Math.max(8, position.top)}px;left:${Math.max(8, position.left)}px`} onSubmit=${run}>
        <strong>${firstLine(summary) || summary.slice(0, 80) || 'Empty note'}</strong>
        <label>Repository<input list="notes-repos" value=${repo} onInput=${e => setRepo(e.target.value)} placeholder="owner/repo" required /></label>
        <datalist id="notes-repos">${repos.map(value => html`<option value=${value} />`)}</datalist>
        ${error ? html`<span class="err">${error}</span>` : null}
        <button class="btn pri" disabled=${busy} type="submit">${busy ? 'Filing…' : 'File ticket'}</button>
      </form>` : null}</span>
    ${pending ? html`<div class="note-activity-retry" role="status">${error}<button type="button" class="btn sm" disabled=${busy} onClick=${retryActivity}>Save activity</button></div>`
      : error && !popover ? html`<span class="err" role="alert">${error}</span>` : null}
  </div>`;
}

export function NotesView({ pane = false, onClose, onType, target }) {
  const activities = useActivityPreviews();
  const [activityFocus, setActivityFocus] = useState(null);
  const [compact, setCompact] = useState(() => window.innerWidth < 900);
  const [query, setQuery] = useState('');
  const [hits, setHits] = useState([]);
  const [total, setTotal] = useState(0);
  const [open, setOpen] = useState(null);
  const [body, setBody] = useState('');
  const [status, setStatus] = useState('');
  const [conflict, setConflict] = useState(null);
  const [historyRows, setHistoryRows] = useState(null);
  const [preview, setPreview] = useState(false);
  const [previewHtml, setPreviewHtml] = useState('');
  const [error, setError] = useState('');
  const [repos, setRepos] = useState([]);
  const editor = useRef(null);
  const file = useRef(null);
  const saveTimer = useRef(null);
  const searchSerial = useRef(0);
  const loadSerial = useRef(0);
  const editSerial = useRef(0);
  const queryRef = useRef(query);
  const current = useRef({ open, body }); current.current = { open, body };
  const saving = useRef(Promise.resolve(true));
  const saveRef = useRef(null);
  const changeQuery = value => { queryRef.current = value; setQuery(value); };
  const search = async (q = query) => {
    const serial = ++searchSerial.current;
    try {
      const rows = await api(`/notes/search?q=${encodeURIComponent(q)}`);
      if (serial !== searchSerial.current || q !== queryRef.current) return;
      setHits(rows);
      if (!q) setTotal(rows.length);
      setError('');
    } catch (err) { if (serial === searchSerial.current && q === queryRef.current) setError(err.message); }
  };
  useEffect(() => { const timer = setTimeout(() => search(query), 150); return () => clearTimeout(timer); }, [query]);
  useEffect(() => {
    if (!preview) return;
    let active = true;
    const timer = setTimeout(() => api('/notes/preview', { method: 'POST', body: { body } })
      .then(result => { if (active) setPreviewHtml(result.html); })
      .catch(err => { if (active) setError(err.message); }), 150);
    return () => { active = false; clearTimeout(timer); };
  }, [preview, body]);
  useEffect(() => { const change = () => setCompact(window.innerWidth < 900); window.addEventListener('resize', change); return () => window.removeEventListener('resize', change); }, []);
  useEffect(() => { const id = stored('sm-notes-open', ''); if (id) load(id); }, []);
  useEffect(() => {
    api('/client/board').then(board => {
      const found = new Set();
      for (const lane of board.lanes || []) {
        if (lane.goal?.repo) found.add(lane.goal.repo);
        for (const ticket of lane.tickets || []) if (ticket.repo) found.add(ticket.repo);
      }
      for (const group of board.other || []) {
        if (group.repo) found.add(group.repo);
        for (const ticket of group.tickets || []) if (ticket.repo) found.add(ticket.repo);
      }
      setRepos([...found].sort());
    }).catch(() => {});
  }, []);
  const load = async id => {
    if (!id) return;
    const serial = ++loadSerial.current;
    const editAtStart = editSerial.current;
    const before = current.current;
    try {
      const note = await api(notePath(id));
      if (serial !== loadSerial.current || editAtStart !== editSerial.current
        || current.current.open?.id !== before.open?.id
        || current.current.open?.version !== before.open?.version
        || current.current.body !== before.body) return;
      current.current = { open: note, body: note.body };
      setOpen(note); setBody(note.body); setStatus(`Saved · ${age(note.updated_at)}`);
      setConflict(null); setHistoryRows(null); setPreview(false); setPreviewHtml('');
    } catch (err) { setError(err.message); }
  };
  useEffect(() => {
    const visible = () => {
      if (document.hidden || !current.current.open) return;
      if (current.current.body === current.current.open.body) load(current.current.open.id);
      else {
        const openAtStart = current.current.open;
        const serial = loadSerial.current;
        api(notePath(openAtStart.id)).then(note => {
          if (serial === loadSerial.current && current.current.open?.id === openAtStart.id
            && current.current.open.version === openAtStart.version
            && current.current.body !== current.current.open.body
            && note.version !== openAtStart.version) setConflict(note);
        }).catch(() => {});
      }
    };
    document.addEventListener('visibilitychange', visible);
    return () => document.removeEventListener('visibilitychange', visible);
  }, []);
  const save = (forceVersion = null) => {
    clearTimeout(saveTimer.current);
    if (conflict && forceVersion === null) return Promise.resolve(false);
    const { open: note, body: text } = current.current;
    if (!note || (text === note.body && forceVersion === null)) return saving.current;
    saving.current = saving.current.then(async () => {
      const version = forceVersion ?? current.current.open?.version;
      if (!version) return false;
      const response = await fetch(notePath(note.id), { method: 'PUT', credentials: 'same-origin',
        headers: { ...clientNav.headers(), 'Content-Type': 'application/json' }, body: JSON.stringify({ body: text, if_version: version }) });
      const result = await response.json();
      if (response.status === 409) { setConflict(result); setStatus('Conflict'); return false; }
      if (!response.ok) throw new Error(result.detail || `HTTP ${response.status}`);
      if (current.current.open?.id !== note.id) return true;
      current.current.open = result;
      setOpen(result); setConflict(null); setStatus(`Saved · ${age(result.updated_at)}`);
      search(queryRef.current);
      return true;
    }).catch(err => { setStatus('Save failed'); setError(err.message); return false; });
    return saving.current;
  };
  saveRef.current = save;
  useEffect(() => bus.on('notes-before-leave', async () => {
    for (;;) {
      if (await saveRef.current() === false) return false;
      const { open: note, body: text } = current.current;
      if (!note || text === note.body) return true;
    }
  }), []);
  const collapse = async () => {
    for (;;) {
      if (await save() === false) return;
      const { open: note, body: text } = current.current;
      if (!note || text === note.body) break;
    }
    loadSerial.current++;
    current.current = { open: null, body: '' };
    store('sm-notes-open', '');
    setOpen(null); setBody('');
  };
  useEffect(() => {
    const key = e => {
      if (e.key === 'Escape' && pane && current.current.open && !e.target.closest('.notes-popover')) {
        e.preventDefault(); e.stopPropagation();
        collapse();
      }
    };
    document.addEventListener('keydown', key);
    return () => document.removeEventListener('keydown', key);
  }, [pane, conflict, open, body]);
  const edit = text => {
    editSerial.current++;
    current.current.body = text; setBody(text); setStatus('Unsaved');
    clearTimeout(saveTimer.current); saveTimer.current = setTimeout(() => save(), 800);
  };
  const choose = async id => { if (await save() === false) return; store('sm-notes-open', id); load(id); };
  const create = async () => {
    if (await save() === false) return;
    try { const note = await api('/notes', { method: 'POST', body: { body: '' } }); changeQuery(''); await search(''); load(note.id); }
    catch (err) { setError(err.message); }
  };
  const importFile = async e => {
    const picked = e.target.files?.[0]; if (!picked) return;
    if (await save() === false) { e.target.value = ''; return; }
    const form = new FormData(); form.append('file', picked);
    try {
      const response = await fetch('/notes/import', { method: 'POST', credentials: 'same-origin', headers: clientNav.headers(), body: form });
      const result = await response.json();
      if (!response.ok) throw new Error(result.detail || `HTTP ${response.status}`);
      changeQuery(''); await search(''); if (result.ids?.[0]) load(result.ids[0]); toast(`Imported ${result.ids.length} notes`);
    } catch (err) { setError(err.message); }
    e.target.value = '';
  };
  const loadHistory = async () => {
    try { setHistoryRows(await api(`${notePath(open.id)}/revisions`)); }
    catch (err) { setError(err.message); }
  };
  const restore = async version => {
    if (await save() === false) return;
    try { const note = await api(`${notePath(open.id)}/restore`, { method: 'POST', body: { version } });
      loadSerial.current++;
      current.current = { open: note, body: note.body }; setOpen(note); setBody(note.body);
      setHistoryRows(null); setStatus('Saved · now'); search();
    } catch (err) { setError(err.message); }
  };
  // On the Notes page, New note is the top bar's main action (2543 C).
  const creator = useRef(create);
  creator.current = create;
  useEffect(() => (pane ? undefined : bus.on('notes-new', () => creator.current())), [pane]);
  const selection = () => selectedText(editor.current, current.current.body);
  const cardText = id => async () => (await api(notePath(id))).body;
  const transfer = async () => { if (await save() === false) return; store('sm-notes-open', open?.id || ''); if (pane) navigate('/notes'); else openPanel('notes:view'); };
  const pinned = query && (pane || compact) && open && !hits.some(hit => hit.id === open.id)
    ? { id: open.id, title: open.title, updated_at: open.updated_at, snippet: body, matches: [] } : null;
  return html`<section class=${`notes-view ${pane ? 'notes-pane' : 'notes-page'}`}>
    <header class="notes-head"><h1>Notes</h1><div class="notes-tools">
      <input type="search" aria-label="Search notes" placeholder="Search notes…" value=${query} onInput=${e => changeQuery(e.target.value)} />
      ${pane ? html`<button type="button" class="btn pri" onClick=${create}>+ New</button>` : null}
      <button type="button" class="btn" onClick=${() => file.current?.click()}>Import</button>
      <input ref=${file} hidden type="file" accept=".md,.txt,text/markdown,text/plain" onChange=${importFile} />
      <button type="button" class="btn" title=${pane ? 'Open on page' : 'Send to pane'} onClick=${transfer}>${pane ? '⤢' : '⇥'}</button>
      ${pane ? html`<button type="button" class="btn" title="Close notes" onClick=${onClose}>×</button>` : null}
    </div></header>
    <div class="notes-count">${hits.length} of ${total} notes</div>
    ${error ? html`<p class="err">${error}</p>` : null}
    <div class="notes-columns"><div class="notes-list">
      ${(pinned ? [pinned, ...hits] : hits).map(hit => html`<article key=${hit.id} class=${`note-card ${open?.id === hit.id ? 'selected' : ''}`}>
        <button class="note-card-main" type="button" onClick=${() => open?.id === hit.id && pane ? collapse() : choose(hit.id)}>
          <span class="note-title">${hit.title || 'Untitled'}${pinned?.id === hit.id ? ' · Open note' : ''}</span><small>${age(hit.updated_at)}</small>
          <span class="note-snippet"><${Snippet} hit=${hit} /></span>
        </button>
        <${NoteActions} noteId=${hit.id} target=${target} text=${() => open?.id === hit.id ? selection() : cardText(hit.id)()} summary=${open?.id === hit.id ? selection() : hit.title} canType=${!!onType}
          onType=${onType} repos=${repos} />
        <${ActivitySummary} event=${activities[hit.id]} onClick=${async () => { await choose(hit.id); setActivityFocus(previous => ({ id: hit.id, token: (previous?.token || 0) + 1 })); }} />
        ${(pane || compact) && open?.id === hit.id ? editorView() : null}
      </article>`)}
      ${!hits.length && !pinned ? html`<p class="notes-empty">${query ? 'No matching notes.' : `No notes yet. Choose ${pane ? '+ New' : 'New note'}.`}</p>` : null}
    </div>
    ${!pane && !compact ? html`<div class="notes-editor-slot">${open ? editorView() : html`<p class="notes-empty">Choose a note or create one.</p>`}</div>` : null}</div>
    ${conflict ? html`<div class="notes-dialog-backdrop"><div class="notes-dialog" role="dialog" aria-modal="true" aria-label="Changed on another device">
      <h2>Changed on another device</h2><p>Choose which version to keep.</p>
      <button class="btn" onClick=${() => { loadSerial.current++; saving.current = Promise.resolve(true); current.current = { open: conflict, body: conflict.body }; setOpen(conflict); setBody(conflict.body); setConflict(null); setStatus('Saved · now'); }}>Load theirs</button>
      <button class="btn pri" onClick=${() => { const version = conflict.version; setConflict(null); save(version); }}>Keep mine</button>
    </div></div>` : null}
  </section>`;

  function editorView() {
    return html`<div class="notes-editor">
      <div class="notes-editor-bar"><strong>${open.title || 'Untitled'}</strong><span class="notes-status">${status}</span>
        <button type="button" class="btn sm" onClick=${() => setPreview(!preview)}>${preview ? 'Edit' : 'Preview'}</button>
        <button type="button" class="btn sm" onClick=${historyRows ? () => setHistoryRows(null) : loadHistory}>History</button>
      </div>
      ${historyRows ? html`<div class="notes-history">${historyRows.map(row => html`<button type="button" onClick=${() => restore(row.version)}>Restore version ${row.version} · ${new Date(row.at).toLocaleString()}</button>`)}</div>` : null}
      ${preview ? html`<div class="notes-preview md" dangerouslySetInnerHTML=${{ __html: previewHtml }} />`
        : html`<textarea ref=${editor} spellcheck="false" aria-label="Note text" value=${body}
            onInput=${e => edit(e.target.value)} onBlur=${() => save()} onKeyDown=${e => {
              if (e.key === 'Tab') { e.preventDefault(); const t = e.target; const at = t.selectionStart;
                const next = t.value.slice(0, at) + '  ' + t.value.slice(t.selectionEnd);
                edit(next); requestAnimationFrame(() => { t.selectionStart = t.selectionEnd = at + 2; }); }
            }} />`}
      <${NoteActions} key=${open.id} noteId=${open.id} target=${target} text=${selection} summary=${selection()} canType=${!!onType} onType=${onType} repos=${repos} />
      <${NoteActivity} key=${open.id} noteId=${open.id} preview=${activities[open.id]} reveal=${activityFocus?.id === open.id ? activityFocus.token : 0} />
    </div>`;
  }
}
