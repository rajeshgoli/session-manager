import { useState } from 'preact/hooks';
import { html, api, usePoll, openPanel } from './ui.js';
import { WorkLinks } from './history.js';
export function GuestbookPage() {
  const [repo, setRepo] = useState('');
  const [before, setBefore] = useState('');
  const [data, error] = usePoll(() => api(`/guestbook?format=json&repo=${encodeURIComponent(repo)}&before=${encodeURIComponent(before)}`), 30000, [repo,before]);
  return html`<div class="content"><label class="list-filter">Repository <input placeholder="All repositories" value=${repo} onInput=${e => {setRepo(e.target.value);setBefore('');}} /></label>
    ${error ? html`<p role="alert">${error.message}</p>` : null}
    ${!data ? html`<p>Loading…</p>` : !data.entries.length ? html`<p class="empty">No guestbook entries.</p>` : null}
    ${(data?.entries || []).map(entry => html`<article class="history-card">
      <div class="history-heading"><button class="text-button" onClick=${() => openPanel(`agent:${entry.session_id}`)}>${entry.session_name}</button><span>${entry.provider}</span><small>${entry.signed_at}</small></div>
      <p class="guestbook-text">${entry.text}</p><small>${entry.repos.join(' · ')}</small><${WorkLinks} work=${{tickets:entry.claims}} />
    </article>`)}
    <div class="list-pagination">${before ? html`<button class="btn" onClick=${() => setBefore('')}>Newest</button>` : null}${data?.next_before ? html`<button class="btn" onClick=${() => setBefore(data.next_before)}>Older →</button>` : null}</div>
  </div>`;
}
