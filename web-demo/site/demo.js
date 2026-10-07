// The demo's page side: start the service worker (sw.js) that answers the
// UI's requests from the recording, then load the app; show the banner and
// the read-only notice. The app shell loads this with `data-app` naming the
// app module; recorded doc pages load it without.
(function () {
  'use strict';
  const REPO_URL = 'https://github.com/rajeshgoli/session-manager';
  const INSTALL_URL = `${REPO_URL}/blob/main/docs/product/operator_guide.md#install-on-macos`;
  const me = document.currentScript;
  const appModule = me && me.dataset.app;
  const topLevel = window.top === window;

  function el(tag, attrs, children) {
    const node = document.createElement(tag);
    Object.assign(node, attrs || {});
    (children || []).forEach((child) => node.append(child));
    return node;
  }

  // ---- banner ----------------------------------------------------------------

  let chapterNode = null;
  function banner() {
    document.documentElement.classList.add('sm-demo');
    chapterNode = el('span', { className: 'sm-demo-chapter' });
    const restart = el('button', { className: 'sm-demo-restart', type: 'button', title: 'Play the recording from the start', textContent: '↺' });
    restart.onclick = () => fetch('/__demo/restart', { method: 'POST' }).then(() => location.reload());
    const bar = el('div', { className: 'sm-demo-bar', role: 'note' }, [
      el('span', { className: 'sm-demo-text', textContent: 'A recorded sprint by a scripted team. ' }),
      el('a', { href: REPO_URL, target: '_blank', rel: 'noopener', textContent: 'See the code' }),
      chapterNode, restart,
    ]);
    document.body.prepend(bar);
    tickChapter();
    setInterval(tickChapter, 5000);
  }

  function clockText(seconds) {
    const s = Math.floor(seconds);
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  }

  function tickChapter() {
    fetch('/__demo/clock').then((r) => (r.ok ? r.json() : null)).then((clock) => {
      if (clock && chapterNode) chapterNode.textContent = `${clockText(clock.t)} / ${clockText(clock.duration)} · ${clock.chapter}`;
    }).catch(() => {});
  }

  // ---- read-only notice ----------------------------------------------------------

  let toastNode = null;
  let toastTimer = 0;
  function toast(text) {
    if (!toastNode) {
      toastNode = el('div', { className: 'sm-demo-toast', role: 'status' });
      document.body.append(toastNode);
    }
    // The notice says what the real app would do; a link names where to get it.
    toastNode.replaceChildren(text.replace(/[;.]?\s*Install Session Manager[^.]*\.?$/i, '. '),
      ...(/Install Session Manager/i.test(text)
        ? [el('a', { href: INSTALL_URL, target: '_blank', rel: 'noopener', textContent: 'Install Session Manager →' })] : []));
    toastNode.classList.add('show');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => toastNode.classList.remove('show'), 6000);
  }

  // ---- boot ------------------------------------------------------------------

  function fail(text) {
    document.getElementById('app')?.replaceChildren(el('p', { className: 'sm-demo-fail', textContent: text }));
  }

  function loadApp() {
    const script = el('script', { type: 'module', src: appModule });
    document.head.append(script);
  }

  // A doc page's own review bar lives in a shadow root the stylesheet can't
  // reach; move it below the banner (and reader-bar.js's bar, when present).
  function offsetDocBar() {
    const review = document.getElementById('sm-doc-ui')?.shadowRoot;
    if (!review) return;
    const readerBar = document.getElementById('sm-reader-bar') ? ' + 36px' : '';
    review.append(el('style', { textContent: `.bar{top:calc(var(--sm-demo-bar, 26px)${readerBar})!important}@media print{.bar{top:0!important}}` }));
  }

  function start() {
    // A doc page inside the reader's iframe sits under the app's banner.
    if (!appModule && !topLevel) return;
    banner();
    if (!appModule) window.addEventListener('load', offsetDocBar, { once: true });
    if (!('serviceWorker' in navigator)) return;
    navigator.serviceWorker.addEventListener('message', (event) => {
      if (event.data && event.data.type === 'sm-demo-read-only') toast(event.data.text);
    });
    if (!appModule) return;
    if (navigator.serviceWorker.controller) { loadApp(); return; }
    // First visit: the app's first request must already go to the worker.
    // A first visit to a reader path got the shell (the host's 404.html);
    // reload so the worker answers it with the recorded page.
    const reader = /^\/(docs|messages|t)\//.test(location.pathname);
    navigator.serviceWorker.addEventListener('controllerchange', () => (reader ? location.reload() : loadApp()), { once: true });
    navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch((error) => fail(`The demo could not start: ${error.message}`));
  }

  if (appModule && !('serviceWorker' in navigator)) {
    document.addEventListener('DOMContentLoaded', () => fail('This demo needs a browser with service workers (not a private window in some browsers).'));
  }
  if (document.body) start();
  else document.addEventListener('DOMContentLoaded', start);
})();
