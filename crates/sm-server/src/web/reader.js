// Same-origin document reader; the document keeps its existing review sheet.
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, registerPanel, closePanel, openPanel, bus } from './ui.js';

export function shellName(path = location.pathname) {
  return ({ '/': 'Agents', '/inbox': 'Inbox', '/board': 'Board', '/history': 'History', '/history/agents': 'History', '/guestbook': 'Guestbook', '/queue': 'Queue' })[path] || 'sm';
}
export function fullScreen(path) {
  const url = new URL(path, location.origin);
  if (url.origin !== location.origin) return;
  url.searchParams.set('from', location.pathname + location.search);
  location.assign(url.pathname + url.search + url.hash);
}
export function readerPath(value) {
  try {
    const url = new URL(value, location.origin);
    return url.origin === location.origin && /^\/(docs|messages|t)\//.test(url.pathname) ? url.pathname + url.search + url.hash : null;
  } catch (_) { return null; }
}
export function Reader({ id, controls, onBack = closePanel }) {
  const frame = useRef(null);
  const [current, setCurrent] = useState(id);
  const [doc, setDoc] = useState(null);
  const [error, setError] = useState('');
  useEffect(() => { setCurrent(id); setDoc(null); }, [id]);
  const loaded = () => {
    try {
      const win = frame.current.contentWindow;
      const url = new URL(win.location.href);
      if (url.protocol === 'about:') { setError('This link is not a document reader path.'); return; }
      if (url.origin !== location.origin) return;
      setCurrent(url.pathname + url.search);
      setDoc(win.__smDoc ? { ...win.__smDoc.config, prState: win.__smDoc.prState } : { title: win.document.title });
      // Reader links stay inside the shell, including links in authored docs.
      win.document.addEventListener('click', (e) => {
        const link = e.target.closest('a[href]');
        if (!link || e.metaKey || e.ctrlKey || e.shiftKey) return;
        const target = new URL(link.href, url);
        if (target.origin === location.origin && target.pathname.startsWith('/docs/')) {
          if (target.pathname === url.pathname && target.search === url.search && target.hash) return;
          e.preventDefault(); openPanel(`doc:${target.pathname}${target.search}${target.hash}`);
        } else if (target.origin === 'https://github.com') {
          const match = target.pathname.match(/^\/([^/]+\/[^/]+)\/(?:issues|pull)\/(\d+)$/);
          if (match) { e.preventDefault(); openPanel(`ticket:${match[1]}#${match[2]}`); }
        }
      });
      win.document.addEventListener('keydown', (e) => shortcut(e));
    } catch (e) { setError('Unable to open this reader.'); }
  };
  const shortcut = (e) => {
    if (e.key === 'e' && e.target.ownerDocument !== document && !e.metaKey && !e.ctrlKey && !e.altKey && !e.target.closest('input,textarea,select,[contenteditable]')) {
      e.preventDefault(); bus.emit('inbox-done');
    }
    if (e.key === 'f' && !e.metaKey && !e.ctrlKey && !e.altKey && !e.target.closest('input,textarea,select,[contenteditable]')) {
      e.preventDefault(); fullScreen(frame.current?.contentWindow.location.href || current);
    }
  };
  useEffect(() => {
    document.addEventListener('keydown', shortcut);
    return () => document.removeEventListener('keydown', shortcut);
  }, [current]);
  return html`<section class="doc-reader">
    <div class="reader-bar">
      <button class="icon-btn" title="Back" onClick=${onBack}>←</button>
      <span class="reader-title">${shellName()} / ${doc?.title || 'Document'}</span>
      ${doc?.revisions?.length ? html`<select aria-label="Revision" value=${doc.sha} onChange=${e => {
        const revision = doc.revisions.find(r => r.sha === e.target.value);
        if (revision) openPanel(`doc:${revision.path}`);
      }}>${doc.revisions.map(r => html`<option value=${r.sha}>${r.sha.slice(0,7)}</option>`)}</select>` : null}
      ${doc?.prNumber ? html`<button class="btn sm" onClick=${() => {
        const match = doc.prUrl?.match(/github.com\/([^/]+\/[^/]+)\/pull\/(\d+)/);
        if (match) openPanel(`ticket:${match[1]}#${match[2]}`);
      }}>#${doc.prNumber} · ${doc.prState || 'unknown'}</button>` : null}
      ${doc?.docId ? html`<button class="btn sm" onClick=${() => frame.current.contentWindow.__smDoc.openReview()}>Review</button>` : null}
      <button class="icon-btn" title="Full screen (f)" onClick=${() => fullScreen(current)}>⤢</button>
      <a href=${current} target="_blank" rel="noopener" title="Open in new tab">↗</a>${controls}
    </div>
    ${error ? html`<p role="alert">${error}</p>` : null}
    <iframe ref=${frame} src=${readerPath(current) || 'about:blank'} title=${doc?.title || 'Document reader'} onLoad=${loaded}></iframe>
  </section>`;
}
registerPanel('doc', Reader);
