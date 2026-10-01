// Same-origin document reader; the document keeps its existing review sheet.
import { useEffect, useRef, useState } from 'preact/hooks';
import { html, registerPanel, closePanel, openPanel, bus, typingIn } from './ui.js';

export function shellName(path = location.pathname) {
  return ({ '/': 'Agents', '/inbox': 'Inbox', '/board': 'Board', '/history': 'History', '/history/agents': 'History', '/history/tickets': 'History', '/guestbook': 'Guestbook', '/queue': 'Queue' })[path] || 'sm';
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
  const [asking, setAsking] = useState(false);
  const [askTarget, setAskTarget] = useState(null);
  const [target, setTarget] = useState('reader');
  const [items, setItems] = useState([]);
  const [question, setQuestion] = useState('');
  const [quote, setQuote] = useState('');
  const [sending, setSending] = useState(false);
  const [askError, setAskError] = useState('');
  const targetInitialized = useRef(false);
  useEffect(() => { setCurrent(id); setDoc(null); setAsking(false); setAskTarget(null); setItems([]); setQuote(''); targetInitialized.current = false; }, [id]);
  useEffect(() => { setAskTarget(null); setItems([]); targetInitialized.current = false; }, [doc?.docId]);
  const selectedQuote = () => {
    try { return frame.current?.contentWindow?.getSelection()?.toString().trim() || ''; }
    catch (_) { return ''; }
  };
  const openAsk = () => { setQuote(selectedQuote()); setAsking(true); };
  const refreshAsk = async (docId = doc?.docId) => {
    if (!docId) return;
    try {
      const response = await fetch(`/docs/${encodeURIComponent(docId)}/ask-target`);
      if (!response.ok) throw Error(`Ask target: ${response.status}`);
      const info = await response.json();
      setAskTarget(info);
      if (!targetInitialized.current) { setTarget(info.default); targetInitialized.current = true; }
      else setTarget(previous => previous === 'author' && !info.author?.live ? info.default : previous);
      const thread = await fetch(`/inbox/thread/${encodeURIComponent(info.thread_key)}?format=json`);
      if (!thread.ok) throw Error(`Work thread: ${thread.status}`);
      const data = await thread.json();
      setItems((data.items || []).filter(item => item.at >= info.first_published_at));
      setAskError('');
    } catch (e) { setAskError(e.message); }
  };
  useEffect(() => {
    if (!asking || !doc?.docId) return;
    refreshAsk(doc.docId);
    const timer = setInterval(() => refreshAsk(doc.docId), 10000);
    return () => clearInterval(timer);
  }, [asking, doc?.docId]);
  const sendAsk = async () => {
    if (sending || !question.trim()) return;
    setSending(true);
    try {
      const response = await fetch(`/docs/${encodeURIComponent(doc.docId)}/ask`, {
        method: 'POST', headers: { 'content-type': 'application/json', 'x-sm-doc-token': doc.token },
        body: JSON.stringify({ text: question, quote: quote || undefined, target }),
      });
      if (!response.ok) throw Error((await response.text()).slice(0, 240));
      setQuestion(''); setQuote('');
      await refreshAsk(doc.docId);
    } catch (e) { setAskError(e.message); }
    finally { setSending(false); }
  };
  const loaded = () => {
    try {
      const win = frame.current.contentWindow;
      const url = new URL(win.location.href);
      if (url.protocol === 'about:') { setError('This link is not a document reader path.'); return; }
      if (url.origin !== location.origin) return;
      setCurrent(url.pathname + url.search + (url.hash === '#sm-review' ? url.hash : ''));
      const details = win.__smDoc ? { ...win.__smDoc.config, prState: win.__smDoc.prState } : { title: win.document.title };
      setDoc({ ...details, hasAppendix: !!win.document.querySelector('.appendix-divider') });
      if (url.hash === '#sm-review' && win.__smDoc?.openReview) win.__smDoc.openReview();
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
    if ((e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === 'p') {
      e.preventDefault(); printDoc(false); return;
    }
    if (e.key === 'e' && e.target.ownerDocument !== document && !e.metaKey && !e.ctrlKey && !e.altKey && !typingIn(e)) {
      e.preventDefault(); bus.emit('inbox-done');
    }
    if (e.key === 'f' && !e.metaKey && !e.ctrlKey && !e.altKey && !typingIn(e)) {
      e.preventDefault(); fullScreen(frame.current?.contentWindow.location.href || current);
    }
    if (e.key === 'a' && !e.metaKey && !e.ctrlKey && !e.altKey && !typingIn(e) && doc?.docId) {
      e.preventDefault(); openAsk();
    }
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
  useEffect(() => {
    document.addEventListener('keydown', shortcut);
    return () => document.removeEventListener('keydown', shortcut);
  }, [current, doc?.docId]);
  return html`<section class=${`doc-reader${asking ? ' ask-open' : ''}`}>
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
      ${doc ? html`<span class="reader-print"><button class="btn sm" onClick=${() => printDoc(false)}>⎙ ${doc.hasAppendix ? 'Print memo' : 'Print'}</button>${doc.hasAppendix ? html`<button class="btn sm" onClick=${() => printDoc(true)}>Print all</button>` : null}</span>` : null}
      ${doc?.docId ? html`<button class="btn sm" aria-pressed=${asking} onClick=${() => asking ? setAsking(false) : openAsk()}>Ask</button>` : null}
      <button class="icon-btn" title="Full screen (f)" onClick=${() => fullScreen(current)}>⤢</button>
      <a href=${current} target="_blank" rel="noopener" title="Open in new tab">↗</a>${controls}
    </div>
    ${error ? html`<p role="alert">${error}</p>` : null}
    <div class="doc-reader-body">
      <iframe ref=${frame} src=${readerPath(current) || 'about:blank'} title=${doc?.title || 'Document reader'} onLoad=${loaded}></iframe>
      ${asking ? html`<aside class="doc-ask" aria-label="Ask about this doc">
        <div class="doc-ask-head"><strong>Ask about this doc</strong><button class="icon-btn" title="Close Ask" onClick=${() => setAsking(false)}>×</button></div>
        <div class="doc-ask-items">${items.map(item => html`<div class="doc-ask-item" dangerouslySetInnerHTML=${{ __html: item.html || '' }} />`)}</div>
        <div class="doc-ask-compose">
          ${quote ? html`<blockquote>${quote}<button title="Remove quote" onClick=${() => setQuote('')}>×</button></blockquote>` : null}
          <select aria-label="Ask target" value=${target} onChange=${e => setTarget(e.target.value)}>
            ${askTarget?.author?.live ? html`<option value="author">${askTarget.author.name}</option>` : null}
            <option value="reader">${askTarget?.reader?.name || 'Reader agent'}</option>
            ${askTarget?.author?.restorable && !askTarget.author.live ? html`<option value="restore_author">Bring back ${askTarget.author.name} · reloads about ${Math.round((askTarget.author.context_tokens || 0) / 1000)}k tokens</option>` : null}
          </select>
          <textarea rows="3" placeholder="Ask about this doc…" value=${question} onInput=${e => setQuestion(e.target.value)} onKeyDown=${e => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); sendAsk(); } }} />
          <div class="doc-ask-actions">${askError ? html`<span role="alert">${askError}</span>` : null}<button class="btn" disabled=${sending || !question.trim()} onClick=${sendAsk}>Send</button></div>
        </div>
      </aside>` : null}
    </div>
  </section>`;
}
registerPanel('doc', Reader);
