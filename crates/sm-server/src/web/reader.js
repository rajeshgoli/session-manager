// The document view (2543 F): a same-origin document fills the main area under
// a bar that matches the terminal's. The document keeps its own review sheet.
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, openPanel, navigate, readHref, readerPath, openReader, Icon } from './ui.js';
import { openInClaude, SECTION_TONE } from './agents.js';
import { BugButton } from './bug.js';
import { followReaderTheme } from './reader-theme.js';

/**
 * The reader path a `/read/…` address shows: the address minus `/read`, its query
 * without the app's own panel keys, and no fragment (F.1).
 */
export function docPathFrom(pathname, search) {
  const params = new URLSearchParams(search);
  const own = ['open', 'panel', 'at'].filter((key) => params.has(key));
  own.forEach((key) => params.delete(key));
  const query = own.length ? params.toString() : search.replace(/^\?/, '');
  return pathname.slice('/read'.length) + (query ? `?${query}` : '');
}

/** ← and ⌘[ (F.3): back when the previous entry is this app's, else the landing. */
export function docBack(state = history.state) {
  if (Number.isInteger(state?.smHistoryIndex) && state.smHistoryIndex > 0) history.back();
  else navigate('/');
}

/** "Revision k" counts publishes oldest = 1; the server lists them newest first. */
export function revisionNumber(revisions, sha) {
  const index = (revisions || []).findIndex((r) => r.sha === sha);
  return index < 0 ? null : revisions.length - index;
}

/** The Review button counts this revision's draft comments (sm#2568). */
export const reviewLabel = (drafts) => (drafts ? `Review (${drafts})` : 'Review');

export const repliesText = (count) => `${count} ${count === 1 ? 'reply' : 'replies'} on changed passages`;

const when = (value) => {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? String(value || '') : date.toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
};

/** One open menu at a time; a press elsewhere closes it. */
function useMenu() {
  const [open, setOpen] = useState(null);
  useEffect(() => {
    if (!open) return;
    const close = (e) => { if (!e.target.closest?.('.doc-menu')) setOpen(null); };
    document.addEventListener('pointerdown', close);
    return () => document.removeEventListener('pointerdown', close);
  }, [open]);
  return [open, setOpen];
}

/** The author agent (C): a live session opens its terminal, otherwise its History entry. */
export function authorOf(doc, sessions) {
  if (!doc?.authorAgent && !doc?.authorSessionId) return null;
  const session = sessions.find((s) => s.id === doc.authorSessionId)
    || sessions.find((s) => doc.authorAgent && s.name === doc.authorAgent);
  const live = !!session && session.state !== 'stopped' && session.attention?.section !== 'stopped';
  return { id: session?.id || doc.authorSessionId, name: session?.name || doc.authorAgent, session, live };
}

/** `id` is the reader path; the view is keyed by it, so another document remounts. */
export function Reader({ id, watch, moon = null }) {
  const frame = useRef(null);
  useEffect(() => followReaderTheme(frame.current), []);
  // The frame loads once; a panel opened beside it rewrites the address but not the document.
  const [src] = useState(() => readerPath(id));
  const [current, setCurrent] = useState(id);
  const [doc, setDoc] = useState(null);
  const [replies, setReplies] = useState(0);
  // The review client's state, posted whenever it would have redrawn its own bar (sm#2568).
  const [review, setReview] = useState(null);
  const [error, setError] = useState(src ? '' : 'This link is not a document reader path.');
  const [menu, setMenu] = useMenu();
  const sessions = (watch?.[0]?.sessions) || [];

  useEffect(() => {
    if (doc?.title) document.title = `${doc.title} · sm`;
  }, [doc?.title]);
  // The review client posts its unseen replies count when it changes (F.9).
  useEffect(() => {
    const listen = (e) => {
      if (e.origin !== location.origin || e.source !== frame.current?.contentWindow) return;
      if (e.data?.type === 'sm-doc-replies') setReplies(Number(e.data.count) || 0);
      if (e.data?.type === 'sm-doc-bar' && e.data.state) setReview(e.data.state);
    };
    window.addEventListener('message', listen);
    return () => window.removeEventListener('message', listen);
  }, []);

  const loaded = () => {
    try {
      const win = frame.current.contentWindow;
      const url = new URL(win.location.href);
      if (url.protocol === 'about:') return;
      if (url.origin !== location.origin) return;
      setCurrent(url.pathname + url.search);
      const details = win.__smDoc ? { ...win.__smDoc.config, prState: win.__smDoc.prState } : { title: win.document.title };
      setDoc({ ...details, hasAppendix: !!win.document.querySelector('.appendix-divider') });
      setReplies(win.__smDoc?.repliesUnseen || 0);
      setReview(win.__smDoc?.barState?.() || null);
      if (url.hash === '#sm-review' && win.__smDoc?.openReview) win.__smDoc.openReview();
      // Links inside a document stay in the document view, including links in authored docs.
      win.document.addEventListener('click', (e) => {
        const link = e.target.closest('a[href]');
        if (!link || e.metaKey || e.ctrlKey || e.shiftKey) return;
        const target = new URL(link.href, url);
        if (target.origin === location.origin && target.pathname === url.pathname && target.search === url.search && target.hash) {
          // Scroll in place: a fragment navigation would add a history entry that ← and ⌘[ step through.
          const anchor = win.document.getElementById(decodeURIComponent(target.hash.slice(1)));
          if (anchor) { e.preventDefault(); anchor.scrollIntoView(); }
        } else if (readerPath(target.href)) {
          e.preventDefault(); openReader(readerPath(target.href));
        } else if (target.origin === 'https://github.com') {
          const match = target.pathname.match(/^\/([^/]+\/[^/]+)\/(?:issues|pull)\/(\d+)$/);
          if (match) { e.preventDefault(); openPanel(`ticket:${match[1]}#${match[2]}`); }
        }
      });
      win.document.addEventListener('keydown', shortcut);
    } catch (_) { setError('Unable to open this reader.'); }
  };
  const printDoc = (all) => {
    const win = frame.current?.contentWindow;
    if (!win) return;
    try {
      if (win.location.origin !== location.origin) return;
      if (all) {
        win.document.documentElement.setAttribute('data-print', 'all');
        win.addEventListener('afterprint', () => {
          const toggle = win.document.getElementById('memo-print-toggle');
          if (toggle) toggle.click(); // Let the template update its own label and setting.
          else win.document.documentElement.removeAttribute('data-print');
        }, { once: true });
      }
      win.print();
    } catch (_) { /* The frame may have navigated away from the document. */ }
  };
  // ⌘P prints the memo; ⌘[ goes back (G). Both work with focus inside the document too.
  function shortcut(e) {
    if (!e.metaKey && !e.ctrlKey) return;
    if (!e.altKey && e.key.toLowerCase() === 'p') { e.preventDefault(); printDoc(false); }
    else if (e.metaKey && e.key === '[') { e.preventDefault(); docBack(); }
  }
  useEffect(() => {
    document.addEventListener('keydown', shortcut);
    return () => document.removeEventListener('keydown', shortcut);
  }, []);

  const client = (name) => frame.current?.contentWindow?.__smDoc?.[name]?.();
  const info = review ? { ...doc, prNumber: review.prNumber ?? doc?.prNumber, prUrl: review.prUrl ?? doc?.prUrl,
    prState: review.prState ?? doc?.prState, revisions: review.revisions?.length ? review.revisions : doc?.revisions } : doc;
  const canReview = review ? review.canReview : !!doc?.docId;
  const note = review?.note;
  const hold = review?.mergeHold;
  const revision = revisionNumber(info?.revisions, info?.sha);
  const author = authorOf(doc, sessions);
  const linked = author?.session?.remote_control?.url?.startsWith('https://claude.ai/code/');
  const pr = () => {
    const match = info.prUrl?.match(/github.com\/([^/]+\/[^/]+)\/pull\/(\d+)/);
    if (match) openPanel(`ticket:${match[1]}#${match[2]}`);
  };
  return html`<section class="doc-reader">
    <header class="bar doc-bar">
      <button type="button" class="icon-btn" title="Back (⌘[)" aria-label="Back" onClick=${() => docBack()}>←</button>
      <strong class="doc-title" title=${doc?.title || ''}>${doc?.title || 'Document'}</strong>
      ${revision ? html`<span class="anchor doc-menu">
        <button type="button" class="btn sm" aria-haspopup="menu" aria-expanded=${menu === 'revisions'}
          onClick=${() => setMenu(menu === 'revisions' ? null : 'revisions')}>Revision ${revision} ▾</button>
        ${menu === 'revisions' ? html`<div class="pop menu" role="menu">${info.revisions.map((r, i) => html`<button type="button" role="menuitem"
            class=${r.sha === info.sha ? 'on' : ''} onClick=${() => { setMenu(null); if (r.sha !== info.sha) navigate(readHref(r.path)); }}>
            Revision ${info.revisions.length - i} · ${when(r.publishedAt)}${r.reviewRequested ? ' · review asked' : ''}</button>`)}</div>` : null}
      </span>` : null}
      ${info?.prNumber ? html`<button type="button" class="btn sm" onClick=${pr}>#${info.prNumber} · ${info.prState || 'unknown'}</button>` : null}
      ${hold ? html`<span class="doc-hold" title=${`Merge held by ${hold.placed_by}${hold.reason ? `: ${hold.reason}` : ''}`}>⏸</span>` : null}
      ${replies > 0 ? html`<button type="button" class="plain-btn doc-replies" onClick=${() => client('showChangedThreads')}>${repliesText(replies)} · <u>Read</u></button>` : null}
      ${note?.kind === 'failed' ? html`<span class="doc-note red" title=${note.text}><span>${note.text}</span> ·
          <button type="button" class="plain-btn" onClick=${() => client('resubmit')}><u>Submit again</u></button> ·
          <button type="button" class="plain-btn" onClick=${() => client('discardSubmission')}><u>Discard</u></button></span>`
        : note ? html`<span class=${`doc-note ${note.kind === 'sent' ? 'green' : 'muted'}`} role="status">${note.text}</span>` : null}
      <span class="sp"></span>
      ${author ? html`<button type="button" class="plain-btn doc-author" title=${author.live ? `Open ${author.name}'s terminal` : `${author.name} in History`}
          onClick=${() => navigate(author.live ? `/terminal/${encodeURIComponent(author.id)}` : `/history?open=agent:${encodeURIComponent(author.id)}`)}>
          <i class=${`sw-dot ${author.live ? SECTION_TONE[author.session.attention?.section] || 'muted' : 'muted'}`}></i>${author.name}</button>` : null}
      ${linked ? html`<button type="button" class="icon-btn" title="Open in Claude" aria-label="Open in Claude"
          onClick=${() => openInClaude(author.session)}><${Icon} name="external" /></button>` : null}
      ${doc ? html`<button type="button" class="btn sm" onClick=${() => printDoc(false)}>⎙ Print</button>` : null}
      <span class="anchor doc-menu">
        <button type="button" class="icon-btn" title="More" aria-label="More" aria-haspopup="menu" aria-expanded=${menu === 'more'}
          onClick=${() => setMenu(menu === 'more' ? null : 'more')}>⋯</button>
        ${menu === 'more' ? html`<div class="pop menu" role="menu">
          ${doc?.hasAppendix ? html`<button type="button" role="menuitem" onClick=${() => { setMenu(null); printDoc(true); }}>Print all</button>` : null}
          ${doc?.docId && info?.prState === 'open' ? html`<button type="button" role="menuitem" onClick=${() => { setMenu(null); client('toggleHold'); }}>${hold ? 'Release hold' : 'Hold merge'}</button>` : null}
          <button type="button" role="menuitem" onClick=${() => { setMenu(null); window.open(current, '_blank', 'noopener'); }}>Open in new tab</button>
        </div>` : null}
      </span>
      ${moon}
      <${BugButton} page="Document" />
      ${review?.reviewRequested ? html`<button type="button" class="btn sm" onClick=${() => client('dismissReview')}>No review needed</button>` : null}
      ${canReview ? html`<button type="button" class="btn pri" onClick=${() => client('openReview')}>${reviewLabel(review?.drafts)}</button>` : null}
    </header>
    ${error ? html`<p role="alert">${error}</p>` : null}
    <div class="doc-reader-body">
      <iframe ref=${frame} src=${src || 'about:blank'} title=${doc?.title || 'Document'} onLoad=${loaded}></iframe>
    </div>
  </section>`;
}
