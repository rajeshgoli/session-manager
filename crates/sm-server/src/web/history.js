import { useState } from 'preact/hooks';
import { html, api, usePoll, openPanel, registerPanel, Seg, navigate, toast } from './ui.js';

export function WorkLinks({ work = {} }) {
  return html`<div class="work-links">
    ${[...(work.tickets || []), ...(work.prs || [])].map(item => html`<button class="btn sm" onClick=${() => openPanel(`ticket:${item.repo || work.repo}#${item.number}`)}>#${item.number} ${item.title}</button>`)}
    ${(work.docs || []).map(doc => html`<button class="btn sm" onClick=${() => openPanel(`doc:${doc.reader_path}`)}>${doc.title || doc.name}</button>`)}
  </div>`;
}
export function HistoryPage({ path }) {
  const agents = path === '/history/agents';
  return html`<div class="content history-page">
    <${Seg} label="History" value=${agents ? 'agents' : 'tickets'} onChange=${value => navigate(value === 'agents' ? '/history/agents' : '/history')}
      options=${[{value:'tickets',label:'Tickets'},{value:'agents',label:'Agents'}]} />
    <${HistoryList} key=${path} agents=${agents} />
  </div>`;
}
function HistoryList({ agents }) {
  const [filters] = useState(() => new URLSearchParams(location.search));
  const [query, setQuery] = useState(() => filters.get(agents ? 'q' : 'repo') || '');
  const [before, setBefore] = useState(() => filters.get('before') || '');
  const [busy, setBusy] = useState(null);
  const [data, error, reload] = usePoll(() => {
    const params = new URLSearchParams({format:'json', [agents ? 'q' : 'repo']:query, before});
    for (const key of agents ? ['limit'] : ['agent','open','limit']) {
      const value = filters.get(key);
      // `open` also carries legacy shell panel links, which are not filters.
      if (value !== null && (key !== 'open' || !value.includes(':'))) params.set(key, value);
    }
    return api(`${agents ? '/history/agents' : '/history'}?${params}`);
  }, 30000, [query,before]);
  const restore = async agent => {
    if (busy) return;
    setBusy(agent.id);
    try { await api(`/sessions/${encodeURIComponent(agent.id)}/restore`, {method:'POST',body:{}}); toast(`Restored ${agent.name}`, () => openPanel(`agent:${agent.id}`)); reload(); }
    catch (e) { toast(e.message); }
    finally { setBusy(null); }
  };
  const rows = data?.[agents ? 'agents' : 'rows'] || [];
  return html`<label class="list-filter">${agents ? 'Find agent' : 'Repository'} <input value=${query} placeholder=${agents ? 'Name or folder' : 'All repositories'} onInput=${e => {setQuery(e.target.value);setBefore('');}} /></label>
    ${error ? html`<p role="alert">${error.message}</p>` : null}
    ${!data ? html`<p>Loading…</p>` : !rows.length ? html`<p class="empty">No history matches.</p>` : null}
    ${rows.map(row => html`<article class="history-card">
      ${agents ? html`<div class="history-heading"><button class="text-button" onClick=${() => openPanel(`agent:${row.id}`)}>${row.name}</button><span>${row.provider} · ${row.state}</span>
        <button class="btn sm" disabled=${!row.restorable || !!busy} title=${row.unrestorable_reason || 'Restore agent'} onClick=${() => restore(row)}>${busy === row.id ? 'Restoring…' : 'Restore'}</button></div>
        <p>${row.last_status || row.working_dir}</p><small>${row.ended_at}</small><${WorkLinks} work=${row.work} />`
        : html`<button class="text-button" onClick=${() => openPanel(`ticket:${row.repo}#${row.number}`)}>#${row.number} ${row.title}</button>
          <p>${row.repo} · ${row.state} · ${(row.flags || []).join(' · ')}</p><${WorkLinks} work=${row} />`}
    </article>`)}
    <div class="list-pagination">${before ? html`<button class="btn" onClick=${() => setBefore('')}>Newest</button>` : null}
    ${data?.next_before ? html`<button class="btn" onClick=${() => setBefore(data.next_before)}>Older →</button>` : null}</div>`;
}
function Ticket({ id, controls }) {
  const split = id.lastIndexOf('#');
  const slug = id.slice(0,split);
  const repo = slug.split('/').pop();
  const number = id.slice(split+1);
  const [data, error] = usePoll(() => api(`/t/${encodeURIComponent(repo)}/${encodeURIComponent(number)}?format=json`), 30000, [id]);
  const followLink = link => {
    if (!link) return;
    const url = new URL(link, location.origin);
    if (url.origin === location.origin && url.pathname.startsWith('/docs/')) openPanel(`doc:${url.pathname}${url.search}`);
    else {
      const match = url.href.match(/^https:\/\/github.com\/([^/]+\/[^/]+)\/(?:issues|pull)\/(\d+)/);
      if (match) openPanel(`ticket:${match[1]}#${match[2]}`);
    }
  };
  return html`<div class="reader-bar"><strong class="reader-title">#${number} ${data?.item.title || repo}</strong>${controls}</div>
    <div class="content">${error?.status === 404 ? html`<p>No agent has worked on this ticket yet, so sm has no history for it.</p>
      ${slug.includes('/') ? html`<a class="btn" href=${`https://github.com/${slug}/issues/${number}`} target="_blank" rel="noopener">Open on GitHub</a>` : null}`
      : error ? html`<p role="alert">${error.message}</p>` : null}
    ${data ? html`<p>${data.item.state}</p><${WorkLinks} work=${data.item} />
      ${(data.events || []).map(event => html`<article class="history-card"><small>${event.at} · ${event.name || ''}</small><p>${event.text}</p>
        ${event.link ? html`<button class="btn sm" onClick=${() => followLink(event.link)}>Open item</button>` : null}</article>`)}` : !error ? 'Loading…' : null}</div>`;
}
registerPanel('ticket', Ticket);
