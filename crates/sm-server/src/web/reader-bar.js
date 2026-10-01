// Injected only on the browser hostname. Never shown inside a reader frame.
(() => {
  if (window.top !== window || document.getElementById('sm-reader-bar')) return;
  const shellPaths = new Set(['/', '/watch', '/inbox', '/board', '/queue', '/history', '/history/agents', '/guestbook', '/analytics', '/settings']);
  const names = { '/': 'Agents', '/watch': 'Agents', '/inbox': 'Inbox', '/board': 'Board', '/queue': 'Queue', '/history': 'History', '/history/agents': 'History', '/guestbook': 'Guestbook', '/analytics': 'Analytics', '/settings': 'Settings' };
  const safeFrom = value => {
    if (!value) return null;
    try {
      const url = new URL(value, location.origin);
      return url.origin === location.origin && shellPaths.has(url.pathname) ? url : null;
    } catch (_) { return null; }
  };
  const from = safeFrom(new URLSearchParams(location.search).get('from')) || safeFrom(document.referrer);
  const current = new URL(location.href); current.searchParams.delete('from');
  const returnTo = from || new URL('/', location.origin);
  let ref;
  if (location.pathname.startsWith('/docs/')) ref = `doc:${current.pathname}${current.search}`;
  else if (location.pathname.startsWith('/t/')) {
    const parts = location.pathname.split('/'); ref = `ticket:${decodeURIComponent(parts[2])}#${parts[3]}`;
  } else ref = returnTo.searchParams.get('open') || `doc:${current.pathname}${current.search}`;
  returnTo.searchParams.set('open', ref);
  const doc = window.__smDoc;
  const config = doc?.config || {};
  const host = document.createElement('div'); host.id = 'sm-reader-bar';
  host.style.cssText = 'position:fixed;top:0;left:0;right:0;height:36px;z-index:2147483647';
  const root = host.attachShadow({mode:'open'});
  const style = document.createElement('style');
  let theme = 'system'; try {theme = localStorage.getItem('sm-theme') || theme;} catch (_) {}
  const dark = theme === 'dark' || theme === 'system' && matchMedia('(prefers-color-scheme:dark)').matches;
  style.textContent = `*{box-sizing:border-box}nav{height:36px;display:flex;align-items:center;gap:10px;padding:0 10px;background:${dark?'#121219':'#fff'};color:${dark?'#f5f7fa':'#16171b'};border-bottom:1px solid ${dark?'#2b2b37':'#e0e0d9'};font:12px -apple-system,BlinkMacSystemFont,sans-serif}a{color:${dark?'#5ee7ff':'#0b7a90'};text-decoration:none;white-space:nowrap}strong{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}button,select{font:inherit;color:inherit;background:transparent;border:1px solid #8888;border-radius:4px;cursor:pointer}button.print-main{margin-right:-10px;border-top-right-radius:0;border-bottom-right-radius:0}button.print-all{border-top-left-radius:0;border-bottom-left-radius:0;border-left:0}select{max-width:85px}small{white-space:nowrap}@media(max-width:600px){small{display:none}nav{gap:6px}strong{font-size:11px}}`;
  root.append(style);
  const nav = document.createElement('nav'); nav.setAttribute('aria-label','sm reader'); root.append(nav);
  const add = (tag,text,attrs={}) => {
    const el = document.createElement(tag); el.textContent = text;
    for (const [key,value] of Object.entries(attrs)) el.setAttribute(key,value);
    nav.append(el); return el;
  };
  add('a','sm',{href:'/'});
  add('a',`← ${from ? names[from.pathname] : 'sm'}`,{href:returnTo.pathname+returnTo.search});
  add('strong',config.title || document.title);
  if (config.revisions?.length) {
    const select = add('select','',{'aria-label':'Revision'});
    config.revisions.forEach(r => {const option = document.createElement('option'); option.value = r.path; option.textContent = r.sha.slice(0,7); option.selected = r.sha === config.sha; select.append(option);});
    select.onchange = () => {const url = new URL(select.value,location.origin); if (from) url.searchParams.set('from',from.pathname+from.search); location.assign(url);};
  }
  if (config.prNumber) add('small',`#${config.prNumber} · ${doc.prState || 'unknown'}`);
  if (config.authorAgent) add('small',config.authorAgent);
  if (doc?.openReview) { const button = add('button','Review'); button.onclick = () => doc.openReview(); }
  const hasAppendix = !!document.querySelector('.appendix-divider');
  const printDoc = all => {
    if (all) {
      document.documentElement.setAttribute('data-print', 'all');
      window.addEventListener('afterprint', () => document.documentElement.removeAttribute('data-print'), { once: true });
    }
    window.print();
  };
  const printButton = add('button',hasAppendix ? '⎙ Print memo' : '⎙ Print',hasAppendix ? {class:'print-main'} : {});
  printButton.onclick = () => printDoc(false);
  if (hasAppendix) { const allButton = add('button','Print all',{class:'print-all'}); allButton.onclick = () => printDoc(true); }
  add('a','⤡',{href:returnTo.pathname+returnTo.search,title:'Return to two-pane view'});
  document.body.append(host);
  // The original review UI keeps its workflow buttons, below this navigation.
  const review = document.getElementById('sm-doc-ui')?.shadowRoot;
  if (review) {
    const css = document.createElement('style');
    css.textContent = '.bar{top:36px}.bar>.t,.bar>select,.bar>a,.bar>.state,.bar>.agent{display:none!important}';
    review.append(css);
  }
  const spacing = document.createElement('style');
  document.body.classList.add('sm-reader-bar-open');
  spacing.textContent = 'html{scroll-padding-top:80px}body.sm-reader-bar-open{padding-top:36px!important}@media print{#sm-reader-bar{display:none!important}html{scroll-padding-top:0!important}body.sm-reader-bar-open{padding-top:0!important}}';
  document.head.append(spacing);
})();
