import { useState } from 'preact/hooks';
import { html, api, usePoll, openPanel, registerPanel, Seg, navigate, toast } from './ui.js';
import { safeThreadHtml } from './inbox.js';

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
  const [tab, setTab] = useState('github');
  const [github, githubError] = usePoll(() => slug.includes('/')
    ? api(`/client/github/${slug.split('/').map(encodeURIComponent).join('/')}/${encodeURIComponent(number)}`)
    : Promise.resolve(null), 60000, [id]);
  const [history, historyError] = usePoll(() => tab === 'history'
    ? api(`/t/${encodeURIComponent(repo)}/${encodeURIComponent(number)}?format=json`)
    : Promise.resolve(null), 30000, [id, tab]);
  const followLink = (link, event) => {
    if (!link) return;
    const url = new URL(link, location.origin);
    if (url.origin === location.origin && url.pathname.startsWith('/docs/')) {
      event?.preventDefault(); openPanel(`doc:${url.pathname}${url.search}`);
    } else {
      const match = url.href.match(/^https:\/\/github.com\/([^/]+\/[^/]+)\/(?:issues|pull)\/(\d+)/);
      if (match) { event?.preventDefault(); openPanel(`ticket:${match[1]}#${match[2]}`); }
    }
  };
  const githubUrl = github?.url || (slug.includes('/') ? `https://github.com/${slug}/issues/${number}` : null);
  return html`<div class="ticket-reader">
    <div class="reader-bar"><strong class="reader-title">#${number} ${github?.title || history?.item.title || repo}</strong>
      ${githubUrl ? html`<a href=${githubUrl} target="_blank" rel="noopener" title="Open on GitHub">↗</a>` : null}${controls}</div>
    <${Seg} label="Ticket view" value=${tab} onChange=${setTab} options=${[{ value: 'github', label: 'GitHub' }, { value: 'history', label: 'sm history' }]} />
    <div class="ticket-reader-content" onClick=${(event) => { const link = event.target.closest('a[href]'); if (link && !event.metaKey && !event.ctrlKey && !event.shiftKey) followLink(link.href, event); }}>
      ${tab === 'github' ? html`${githubError ? html`<p role="alert">${githubError.message}</p>` : null}
        ${github ? html`<p class="sub">${github.kind === 'pr' ? 'Pull request' : 'Issue'} · ${github.state}${github.state_reason ? ` · ${github.state_reason}` : ''} · ${github.author}</p>
          ${(github.labels || []).length ? html`<p class="ticket-labels">${github.labels.map(label => html`<span class="link-chip">${label}</span>`)}</p>` : null}
          ${github.pr ? html`<p class="sub">${github.pr.head} → ${github.pr.base}${github.pr.draft ? ' · draft' : ''}${github.pr.merged ? ' · merged' : ''}${github.pr.review_decision ? ` · ${github.pr.review_decision}` : ''}</p>
            ${(github.pr.checks || []).length ? html`<section><h3>Checks</h3>${github.pr.checks.map(check => html`<p>${check.name} · ${check.conclusion || 'running'}</p>`)}</section>` : null}` : null}
          <div class="ticket-markdown" dangerouslySetInnerHTML=${{ __html: safeThreadHtml(github.body_html || '') }} />
          <h3>Comments</h3>${(github.comments || []).map(comment => html`<article class="ticket-comment"><small>${comment.author} · ${comment.created_at}</small>
            <div class="ticket-markdown" dangerouslySetInnerHTML=${{ __html: safeThreadHtml(comment.body_html || '') }} /></article>`)}` : !githubError ? 'Loading…' : null}`
        : html`${historyError?.status === 404 ? html`<p>No agent has worked on this ticket yet, so sm has no history for it.</p>`
          : historyError ? html`<p role="alert">${historyError.message}</p>` : null}
          ${history ? html`<p>${history.item.state}</p><${WorkLinks} work=${history.item} />
            ${(history.events || []).map(event => html`<article class="history-card"><small>${event.at} · ${event.name || ''}</small><p>${event.text}</p>
              ${event.link ? html`<button class="btn sm" onClick=${() => followLink(event.link)}>Open item</button>` : null}</article>`)}` : !historyError ? 'Loading…' : null}`}
    </div>
  </div>`;
}
registerPanel('ticket', Ticket);
