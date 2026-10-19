// A note stays a document. Its destinations live in a quiet, expandable footer.
import { useEffect, useState } from 'preact/hooks';
import { html, api, bus, Icon, openPanel, openItem, navigate } from './ui.js';

const path = id => `/notes/${encodeURIComponent(id)}/activity`;
const verb = kind => ({ ticket: 'Filed as ticket', agent_started: 'Started agent', terminal_added: 'Added to terminal' })[kind] || 'Used in';
const icon = kind => kind === 'ticket' ? 'board' : 'terminal';
const date = at => new Date(at).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
export const activityDestination = event => event.kind === 'ticket'
  ? { kind: 'ticket', key: event.target }
  : { kind: 'agent', key: event.target };
const visit = event => {
  const dest = activityDestination(event);
  if (event.kind === 'terminal_added') navigate(`/terminal/${encodeURIComponent(dest.key)}`);
  else if (dest.kind === 'agent') openPanel(`agent:${dest.key}`);
  else {
    const [repo, number] = dest.key.split('#');
    openItem('ticket', dest.key, `https://github.com/${repo}/issues/${number}`);
  }
};

export function useActivityPreviews() {
  const [previews, setPreviews] = useState({});
  useEffect(() => {
    let active = true;
    let serial = 0;
    const load = async () => {
      const attempt = ++serial;
      try {
        const rows = await api('/notes/activity');
        if (active && attempt === serial) setPreviews(Object.fromEntries(rows.map(row => [row.note_id, row])));
      } catch (_) { /* Writing remains available if history cannot be loaded. */ }
    };
    load();
    const off = bus.on('note-activity-changed', load);
    const visible = () => { if (!document.hidden) load(); };
    document.addEventListener('visibilitychange', visible);
    return () => { active = false; off(); document.removeEventListener('visibilitychange', visible); };
  }, []);
  return previews;
}

export function ActivitySummary({ event, onClick }) {
  if (!event) return null;
  return html`<button type="button" class="note-activity-summary" onClick=${onClick}
    title=${`${verb(event.kind)} · ${date(event.at)} · View activity`}>
    <${Icon} name=${icon(event.kind)} size="13" />
    <span>${event.kind === 'ticket' ? `Filed #${event.target.split('#')[1]} · ${event.target.split('#')[0].split('/').pop()}` : `${event.kind === 'terminal_added' ? 'Added to' : 'Started'} ${event.label}`}</span>
    ${event.count > 1 ? html`<small>+${event.count - 1}</small>` : null}
    <${Icon} name="unfold" size="12" />
  </button>`;
}

function ActivityRow({ event, noteId }) {
  const [showText, setShowText] = useState(false);
  const [text, setText] = useState(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const toggle = async () => {
    const next = !showText; setShowText(next);
    if (!next || text !== null || busy) return;
    setBusy(true); setError('');
    try { setText((await api(`${path(noteId)}/${encodeURIComponent(event.id)}`)).body); }
    catch (err) { setError(err.message); }
    finally { setBusy(false); }
  };
  return html`<li class="note-activity-row">
    <span class="note-activity-mark"><${Icon} name=${icon(event.kind)} size="15" /></span>
    <div class="note-activity-entry">
      <div class="note-activity-description"><span>${verb(event.kind)}</span><time dateTime=${event.at} title=${new Date(event.at).toLocaleString()}>${date(event.at)}</time></div>
      <button type="button" class="note-activity-destination" onClick=${() => visit(event)}>${event.label}<${Icon} name="external" size="12" /></button>
      ${event.kind === 'ticket' ? html`<span class="note-activity-context">${event.target}</span>` : null}
      <button type="button" class="note-activity-text-toggle" aria-expanded=${showText} onClick=${toggle}>${showText ? 'Hide text' : 'View text used'}<span> · ${event.chars.toLocaleString()} characters</span></button>
      ${showText ? html`<div class="note-activity-snapshot">${busy ? html`<span>Loading text…</span>` : error ? html`<p role="alert">${error}<button type="button" onClick=${() => { setShowText(false); }}>Close</button></p>` : html`<pre>${text}</pre>`}</div>` : null}
    </div>
  </li>`;
}

export function NoteActivity({ noteId, preview, reveal = 0 }) {
  const [expanded, setExpanded] = useState(!!reveal);
  const [rows, setRows] = useState(null);
  const [error, setError] = useState('');
  const [retry, setRetry] = useState(0);
  useEffect(() => { if (reveal) setExpanded(true); }, [reveal]);
  useEffect(() => {
    if (!expanded) return;
    let active = true;
    setError('');
    api(path(noteId)).then(value => { if (active) setRows(value); }).catch(err => { if (active) setError(err.message); });
    return () => { active = false; };
  }, [noteId, expanded, preview?.id, retry]);
  if (!preview && !rows?.length) return null;
  return html`<section class="note-activity" aria-label="Note activity">
    <button type="button" class="note-activity-heading" aria-expanded=${expanded} onClick=${() => setExpanded(!expanded)}>
      <${Icon} name="history" size="15" /><strong>Activity</strong><span>${preview?.count || rows?.length || ''}</span>
      <span class="note-activity-latest">${preview ? verb(preview.kind) : ''}</span>
      <${Icon} name=${expanded ? 'fold' : 'unfold'} size="13" />
    </button>
    ${expanded ? html`<div class="note-activity-expanded">
      <p class="note-activity-caption">Destinations and the text you used.</p>
      ${error ? html`<p role="alert" class="err">Couldn’t load activity. <button type="button" class="btn sm" onClick=${() => setRetry(retry + 1)}>Try again</button></p>`
        : rows ? html`<ol class="note-activity-list">${rows.map(event => html`<${ActivityRow} key=${event.id} event=${event} noteId=${noteId} />`)}</ol>` : html`<p class="note-activity-caption">Loading activity…</p>`}
    </div>` : null}
  </section>`;
}

export async function recordNoteActivity(noteId, event) {
  await api(path(noteId), { method: 'POST', body: event });
  bus.emit('note-activity-changed', noteId);
}
