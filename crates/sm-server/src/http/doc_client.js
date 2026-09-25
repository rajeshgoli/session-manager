(function (CONFIG) {
  'use strict';
  // sm owner-doc review client (sm#1451). Everything lives under
  // window.__smDoc and a shadow root so the doc's own scripts and styles
  // are untouched.
  if (window.__smDoc) return;
  var S = window.__smDoc = {
    config: CONFIG,
    drafts: (CONFIG.drafts || []).slice(),
    head: null,
    canComment: !!CONFIG.canComment,
    prState: CONFIG.prState,
    submissionId: null
  };
  var coarse = !!(window.matchMedia && window.matchMedia('(pointer: coarse)').matches);
  var BLOCK = '[data-sm-line]';
  var INTERACTIVE = 'a,button,input,select,textarea,label,summary,video,audio,[contenteditable],[onclick],[role=button]';

  function sha7(s) { return (s || '').slice(0, 7); }
  function el(tag, attrs, kids) {
    var e = document.createElement(tag);
    for (var k in attrs || {}) {
      if (k === 'text') e.textContent = attrs[k];
      else if (k.slice(0, 2) === 'on') e.addEventListener(k.slice(2), attrs[k]);
      else e.setAttribute(k, attrs[k]);
    }
    (kids || []).forEach(function (c) { if (c) e.appendChild(typeof c === 'string' ? document.createTextNode(c) : c); });
    return e;
  }
  function clear(e) { while (e.firstChild) e.removeChild(e.firstChild); return e; }
  function collapse(text) { return (text || '').replace(/\s+/g, ' ').trim(); }
  function store(key, value) {
    try { if (value === undefined) return localStorage.getItem(key); localStorage.setItem(key, value); } catch (e) { return null; }
  }
  function newId() {
    if (window.crypto && crypto.randomUUID) { try { return crypto.randomUUID(); } catch (e) { /* insecure context */ } }
    var bytes = new Uint8Array(16);
    (window.crypto || { getRandomValues: function (b) { for (var i = 0; i < b.length; i++) b[i] = Math.random() * 256; return b; } }).getRandomValues(bytes);
    return Array.prototype.map.call(bytes, function (b) { return ('0' + b.toString(16)).slice(-2); }).join('');
  }
  function api(method, suffix, body) {
    var headers = { Accept: 'application/json' };
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    if (CONFIG.token) headers['X-SM-Doc-Token'] = CONFIG.token;
    return fetch('/docs/' + CONFIG.docId + suffix, {
      method: method, headers: headers, credentials: 'same-origin',
      body: body === undefined ? undefined : JSON.stringify(body)
    }).then(function (r) {
      return r.text().then(function (t) {
        var j = null;
        try { j = t ? JSON.parse(t) : null; } catch (e) { j = null; }
        if (!r.ok) { var err = new Error((j && j.detail) || ('HTTP ' + r.status)); err.status = r.status; throw err; }
        return j;
      });
    });
  }
  function revision(sha) {
    return (CONFIG.revisions || []).filter(function (r) { return r.sha === sha; })[0];
  }
  function pathFor(sha) {
    var r = revision(sha);
    if (r) return r.path;
    if (S.head && S.head.pr_head_sha === sha) return S.head.pr_head_reader_path;
    return null;
  }
  function currentDrafts() { return S.drafts.filter(function (d) { return d.commit_sha === CONFIG.sha; }); }

  // ---- shadow UI -------------------------------------------------------
  var host = el('div', { id: 'sm-doc-ui' });
  host.style.cssText = 'all:initial;position:fixed;top:0;left:0;width:0;height:0;z-index:2147483647';
  var root = host.attachShadow ? host.attachShadow({ mode: 'open' }) : host;
  root.appendChild(el('style', { text: [
    ':host{all:initial}',
    '[hidden]{display:none!important}',
    '*{box-sizing:border-box;font:14px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}',
    '.bar{position:fixed;top:0;left:0;right:0;display:flex;flex-wrap:wrap;align-items:center;gap:6px 10px;padding:6px 10px;background:#111827;color:#f9fafb;box-shadow:0 1px 4px rgba(0,0,0,.3)}',
    '.bar .t{font-weight:600;max-width:40vw;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}',
    '.bar a{color:#93c5fd;text-decoration:none}',
    '.bar select{background:#1f2937;color:#f9fafb;border:1px solid #374151;border-radius:4px;padding:2px 4px;max-width:46vw}',
    '.muted{color:#9ca3af}.ro{color:#fbbf24}.sp{flex:1}',
    'button{cursor:pointer;border:0;border-radius:6px;padding:6px 12px;background:#e5e7eb;color:#111827}',
    'button.p{background:#2563eb;color:#fff}button.d{background:#fee2e2;color:#991b1b}button:disabled{opacity:.5;cursor:default}',
    '.bar button{padding:3px 10px}',
    '.pill{position:fixed;top:6px;right:8px;padding:4px 10px;border-radius:14px;background:#111827;color:#f9fafb;box-shadow:0 1px 4px rgba(0,0,0,.3)}',
    '.ban{position:fixed;left:0;right:0;padding:6px 10px;background:#fef3c7;color:#78350f;display:flex;flex-wrap:wrap;gap:6px 12px;align-items:center}',
    '.ban a,.ban .lk{color:#1d4ed8;text-decoration:underline;cursor:pointer;background:none;padding:0}',
    '.chip{position:fixed;background:#2563eb;color:#fff;border-radius:16px;padding:6px 14px;box-shadow:0 2px 8px rgba(0,0,0,.35)}',
    '.sheet{position:fixed;bottom:0;right:0;width:min(440px,100vw);max-height:80vh;overflow:auto;background:#fff;color:#111827;border-radius:10px 10px 0 0;box-shadow:0 -2px 16px rgba(0,0,0,.35);padding:12px;display:flex;flex-direction:column;gap:8px}',
    '.q{border-left:3px solid #d1d5db;padding-left:8px;color:#4b5563;max-height:6em;overflow:auto;white-space:pre-wrap}',
    'textarea{width:100%;min-height:6em;padding:8px;border:1px solid #d1d5db;border-radius:6px;resize:vertical}',
    '.row{display:flex;gap:8px;justify-content:flex-end;flex-wrap:wrap;align-items:center}',
    '.item{border:1px solid #e5e7eb;border-radius:6px;padding:8px;display:flex;flex-direction:column;gap:4px}',
    '.err{color:#b91c1c}.ok{color:#15803d}h3{margin:0;font-weight:600;font-size:15px}',
    'label{display:flex;gap:6px;align-items:center}'
  ].join('\n') }));
  var ui = el('div');
  root.appendChild(ui);
  (document.body || document.documentElement).appendChild(host);

  var collapsed = store('sm-doc-bar-collapsed') === '1';
  var bar = el('div', { class: 'bar' });
  var pill = el('button', { class: 'pill', text: 'sm ▾', onclick: function () { setCollapsed(false); } });
  var banner = el('div', { class: 'ban' });
  var chip = el('button', { class: 'chip' });
  var sheet = el('div', { class: 'sheet' });
  [bar, pill, banner, chip, sheet].forEach(function (e) { ui.appendChild(e); });
  banner.hidden = chip.hidden = sheet.hidden = true;

  function setCollapsed(value) {
    collapsed = value;
    store('sm-doc-bar-collapsed', value ? '1' : '0');
    layout();
  }
  function layout() {
    bar.hidden = collapsed;
    pill.hidden = !collapsed;
    var h = collapsed ? 0 : bar.getBoundingClientRect().height;
    document.documentElement.style.setProperty('--sm-doc-bar', h + 'px');
    document.documentElement.classList.toggle('sm-doc-bar-open', !collapsed);
    banner.style.top = (collapsed ? 40 : h) + 'px';
  }
  window.addEventListener('resize', layout);

  function stateText() {
    if (!CONFIG.prNumber) return ['Read-only: no PR', 'ro'];
    if (S.prState === 'open') return [S.canComment ? 'PR open' : 'Read-only', S.canComment ? 'muted' : 'ro'];
    if (S.prState === 'unknown' || !S.prState) return ['Read-only: PR state unknown', 'ro'];
    return ['Read-only: PR ' + S.prState, 'ro'];
  }

  function renderBar() {
    clear(bar);
    var picker = el('select', { 'aria-label': 'Revision', onchange: function () { if (picker.value && picker.value !== CONFIG.sha) { var p = pathFor(picker.value); if (p) location.href = p; } } });
    var shas = {};
    (CONFIG.revisions || []).forEach(function (r, i) {
      shas[r.sha] = 1;
      picker.appendChild(el('option', { value: r.sha, text: sha7(r.sha) + ' · ' + (r.publishedAt || '').slice(0, 16).replace('T', ' ') + (i === 0 ? ' (latest)' : '') }));
    });
    var head = S.head;
    if (head && head.pr_head_sha && !shas[head.pr_head_sha] && head.pr_head_blob_sha &&
        !(CONFIG.revisions || []).some(function (r) { return r.blobSha === head.pr_head_blob_sha; })) {
      shas[head.pr_head_sha] = 1;
      picker.appendChild(el('option', { value: head.pr_head_sha, text: 'PR head ' + sha7(head.pr_head_sha) + ' (unpublished)' }));
    }
    if (!shas[CONFIG.sha]) picker.appendChild(el('option', { value: CONFIG.sha, text: sha7(CONFIG.sha) }));
    picker.value = CONFIG.sha;
    var st = stateText();
    var n = currentDrafts().length;
    [
      el('span', { class: 't', title: CONFIG.title, text: CONFIG.title }),
      picker,
      CONFIG.prUrl ? el('a', { href: CONFIG.prUrl, target: '_blank', rel: 'noopener', text: 'PR #' + CONFIG.prNumber }) : null,
      el('span', { class: st[1], text: st[0] }),
      el('span', { class: 'sp' }),
      (S.canComment || n) ? el('button', { class: 'p', text: 'Review (' + n + ')', onclick: openReview }) : null,
      el('button', { title: 'Hide', text: '▴', onclick: function () { setCollapsed(true); } })
    ].forEach(function (c) { if (c) bar.appendChild(c); });
    layout();
  }

  // ---- banners ---------------------------------------------------------
  var discardArmed = {};
  function renderBanner() {
    clear(banner);
    var head = S.head;
    var lines = [];
    if (head) {
      var isPublished = !!revision(CONFIG.sha);
      if (head.latest_published_sha && (head.latest_published_sha !== CONFIG.latestSha ||
          (isPublished && head.latest_published_sha !== CONFIG.sha))) {
        lines.push([el('span', { text: 'A newer revision was published.' }), el('a', { href: head.latest_reader_path, text: 'Load it.' })]);
      } else if (head.pr_head_blob_differs && head.pr_head_sha && head.pr_head_sha !== CONFIG.sha && head.pr_head_reader_path) {
        lines.push([el('span', { text: 'The PR has newer unpublished changes to this doc.' }), el('a', { href: head.pr_head_reader_path, text: 'View the PR head' })]);
      }
    }
    var other = {};
    S.drafts.forEach(function (d) { if (d.commit_sha !== CONFIG.sha) (other[d.commit_sha] = other[d.commit_sha] || []).push(d); });
    Object.keys(other).forEach(function (sha) {
      var list = other[sha];
      var path = pathFor(sha);
      var parts = [el('span', { text: list.length + ' draft comment' + (list.length === 1 ? '' : 's') + ' on ' + sha7(sha) })];
      if (path) parts.push(el('a', { href: path, text: 'Submit them against ' + sha7(sha) }));
      parts.push(el('button', { class: 'lk', text: discardArmed[sha] ? 'Tap again to discard' : 'Discard', onclick: function () {
        if (!discardArmed[sha]) { discardArmed[sha] = 1; renderBanner(); return; }
        Promise.all(list.map(function (d) { return api('DELETE', '/drafts/' + d.id).catch(function () {}); })).then(function () {
          S.drafts = S.drafts.filter(function (d) { return d.commit_sha !== sha; });
          delete discardArmed[sha];
          renderBanner();
        });
      } }));
      lines.push(parts);
    });
    lines.forEach(function (parts) { var row = el('div', {}, parts); row.style.cssText = 'display:flex;gap:6px 12px;flex-wrap:wrap;width:100%'; banner.appendChild(row); });
    banner.hidden = !lines.length;
    layout();
  }

  function pollHead() {
    if (document.visibilityState === 'hidden') return;
    api('GET', '/head?sha=' + CONFIG.sha).then(function (head) {
      S.head = head;
      if (head.pr_state) {
        S.prState = head.pr_state;
        if (head.pr_state !== 'unknown') S.canComment = head.pr_state === 'open';
      }
      renderBar();
      renderBanner();
    }).catch(function () { /* offline or signed out: keep what we have */ });
  }

  // ---- draft markers ---------------------------------------------------
  function blocksFor(line) { return document.querySelectorAll('[data-sm-line="' + line + '"]'); }
  function markers() {
    Array.prototype.forEach.call(document.querySelectorAll('[data-sm-drafts]'), function (b) { b.removeAttribute('data-sm-drafts'); });
    var counts = new Map();
    currentDrafts().forEach(function (d) {
      if (d.line == null) return;
      var blocks = blocksFor(d.line);
      var probe = collapse(d.quote).slice(0, 40);
      var target = null;
      for (var i = blocks.length - 1; i >= 0 && !target; i--) if (probe && collapse(blocks[i].textContent).indexOf(probe) >= 0) target = blocks[i];
      target = target || blocks[0];
      if (target) counts.set(target, (counts.get(target) || 0) + 1);
    });
    counts.forEach(function (n, b) { b.setAttribute('data-sm-drafts', String(n)); });
  }

  // ---- selecting what to comment on -------------------------------------
  var anchor = null; // {line, quote, rect()}
  var selected = null;
  function unselect() { if (selected) selected.removeAttribute('data-sm-selected'); selected = null; }
  function hideChip() { chip.hidden = true; anchor = null; unselect(); }
  function lineOf(node) {
    var e = node && (node.nodeType === 1 ? node : node.parentElement);
    var b = e && e.closest && e.closest(BLOCK);
    return b ? parseInt(b.getAttribute('data-sm-line'), 10) : null;
  }
  function placeChip() {
    if (!anchor) return;
    var r = anchor.rect();
    if (coarse) { chip.style.left = '50%'; chip.style.transform = 'translateX(-50%)'; chip.style.top = ''; chip.style.bottom = '16px'; }
    else {
      chip.style.transform = ''; chip.style.bottom = '';
      chip.style.top = Math.max(8, Math.min(window.innerHeight - 44, r.bottom + 6)) + 'px';
      chip.style.left = Math.max(8, Math.min(window.innerWidth - 120, r.right - 60)) + 'px';
    }
    chip.hidden = false;
  }
  function offer(a, label) {
    if (!S.canComment) return;
    anchor = a;
    chip.textContent = label;
    chip.onclick = function (e) { e.stopPropagation(); var a2 = anchor; hideChip(); compose(a2); };
    placeChip();
  }
  function fromSelection() {
    var sel = window.getSelection && window.getSelection();
    if (!sel || sel.isCollapsed || !sel.rangeCount) return false;
    var range = sel.getRangeAt(0);
    var text = sel.toString().trim();
    if (!text || host.contains(range.commonAncestorContainer)) return false;
    unselect();
    offer({ line: lineOf(range.startContainer), quote: text, rect: function () { return range.getBoundingClientRect(); } }, 'Comment');
    return true;
  }
  document.addEventListener('mouseup', function (e) {
    if (e.target === host) return;
    setTimeout(function () { if (!fromSelection() && !coarse) hideChip(); }, 0);
  });
  var selTimer = null;
  document.addEventListener('selectionchange', function () {
    if (!coarse) return;
    clearTimeout(selTimer);
    selTimer = setTimeout(fromSelection, 350);
  });
  document.addEventListener('click', function (e) {
    if (!coarse || e.target === host) return;
    var sel = window.getSelection && window.getSelection();
    if (sel && !sel.isCollapsed) return;
    var t = e.target;
    if (!t.closest || t.closest(INTERACTIVE)) return;
    var block = t.closest(BLOCK);
    if (!block || block === selected) { hideChip(); return; }
    unselect();
    selected = block;
    block.setAttribute('data-sm-selected', '');
    offer({
      line: parseInt(block.getAttribute('data-sm-line'), 10),
      quote: collapse(block.textContent).slice(0, 300),
      rect: function () { return block.getBoundingClientRect(); }
    }, 'Comment on this');
  });
  window.addEventListener('scroll', function () { if (!coarse && anchor) placeChip(); }, { passive: true });

  // ---- composer ----------------------------------------------------------
  function closeSheet() { sheet.hidden = true; clear(sheet); }
  function compose(a, draft) {
    clear(sheet);
    var box = el('textarea', { placeholder: 'Comment' });
    if (draft) box.value = draft.body;
    var msg = el('div', { class: 'err' });
    var save = el('button', { class: 'p', text: draft ? 'Save' : 'Add draft', onclick: function () {
      var body = box.value.trim();
      if (!body) { msg.textContent = 'Write a comment first.'; return; }
      save.disabled = true;
      var done = draft
        ? api('PATCH', '/drafts/' + draft.id, { body: body }).then(function (d) { S.drafts = S.drafts.map(function (x) { return x.id === d.id ? d : x; }); })
        : api('POST', '/drafts', { sha: CONFIG.sha, line: a.line, quote: a.quote, body: body }).then(function (d) { S.drafts.push(d); });
      done.then(function () { closeSheet(); markers(); renderBar(); }).catch(function (err) { save.disabled = false; msg.textContent = err.message; });
    } });
    var quote = draft ? draft.quote : a.quote;
    [
      el('h3', { text: draft ? 'Edit comment' : 'New comment' }),
      quote ? el('div', { class: 'q', text: quote }) : null,
      (draft ? draft.line : a.line) == null ? el('div', { class: 'muted', text: 'Not tied to a line: posts as a comment on the file.' }) : null,
      box, msg,
      el('div', { class: 'row' }, [
        draft ? el('button', { class: 'd', text: 'Delete', onclick: function () {
          api('DELETE', '/drafts/' + draft.id).then(function () {
            S.drafts = S.drafts.filter(function (x) { return x.id !== draft.id; });
            closeSheet(); markers(); renderBar();
          }).catch(function (err) { msg.textContent = err.message; });
        } }) : null,
        el('button', { text: 'Cancel', onclick: closeSheet }),
        save
      ])
    ].forEach(function (c) { if (c) sheet.appendChild(c); });
    sheet.hidden = false;
    box.focus();
  }

  // ---- review panel ------------------------------------------------------
  function openReview() {
    hideChip();
    clear(sheet);
    if (!S.submissionId) S.submissionId = newId();
    var drafts = currentDrafts();
    sheet.appendChild(el('h3', { text: 'Review ' + sha7(CONFIG.sha) + ' (' + drafts.length + ' comment' + (drafts.length === 1 ? '' : 's') + ')' }));
    drafts.forEach(function (d) {
      sheet.appendChild(el('div', { class: 'item' }, [
        d.quote ? el('div', { class: 'q', text: d.quote.length > 160 ? d.quote.slice(0, 160) + '…' : d.quote }) : null,
        el('div', { text: d.body }),
        el('div', { class: 'row' }, [
          d.line != null ? el('button', { text: 'Show', onclick: function () { var b = blocksFor(d.line)[0]; if (b) { closeSheet(); b.scrollIntoView({ block: 'center' }); } } }) : null,
          el('button', { text: 'Edit', onclick: function () { compose(null, d); } })
        ])
      ]));
    });
    var msg = el('div');
    if (!S.canComment) {
      sheet.appendChild(el('div', { class: 'ro', text: stateText()[0] + '. These drafts cannot be submitted.' }));
      sheet.appendChild(el('div', { class: 'row' }, [el('button', { text: 'Close', onclick: closeSheet })]));
      sheet.hidden = false;
      return;
    }
    var verdicts = [['approve', 'Approve'], ['changes_requested', 'Request changes'], ['comment', 'Comment']];
    var radios = verdicts.map(function (v, i) {
      var r = el('input', { type: 'radio', name: 'sm-verdict', value: v[0] });
      if (i === 2) r.checked = true;
      return el('label', {}, [r, v[1]]);
    });
    var body = el('textarea', { placeholder: 'Overall comment (optional)' });
    var submit = el('button', { class: 'p', text: 'Submit review', onclick: function () {
      var picked = root.querySelector ? root.querySelector('input[name=sm-verdict]:checked') : null;
      submit.disabled = true;
      msg.className = 'muted';
      msg.textContent = 'Submitting…';
      api('POST', '/review', { submission_id: S.submissionId, sha: CONFIG.sha, verdict: picked ? picked.value : 'comment', body: body.value })
        .then(function (res) {
          S.submissionId = null;
          S.drafts = S.drafts.filter(function (d) { return d.commit_sha !== CONFIG.sha; });
          markers(); renderBar();
          clear(sheet);
          sheet.appendChild(el('h3', { class: 'ok', text: 'Review posted' }));
          sheet.appendChild(el('a', { href: res.github_review_url, target: '_blank', rel: 'noopener', text: 'Open it on GitHub' }));
          sheet.appendChild(el('div', { class: 'row' }, [el('button', { text: 'Close', onclick: closeSheet })]));
        })
        .catch(function (err) {
          // A server answer means this submission is finished (failed or
          // refused), so the next attempt is a new one. A network failure
          // may have reached the server: retry with the same id.
          if (err.status) S.submissionId = newId();
          submit.disabled = false;
          msg.className = 'err';
          msg.textContent = err.message + (err.status ? '' : ' (retrying is safe)');
        });
    } });
    [el('div', { class: 'row' }, radios), body, msg,
      el('div', { class: 'row' }, [el('button', { text: 'Cancel', onclick: closeSheet }), submit])
    ].forEach(function (c) { sheet.appendChild(c); });
    sheet.hidden = false;
  }

  document.addEventListener('keydown', function (e) { if (e.key === 'Escape') { closeSheet(); hideChip(); } });
  renderBar();
  renderBanner();
  markers();
  pollHead();
  setInterval(pollHead, 60000);
  document.addEventListener('visibilitychange', pollHead);
})
