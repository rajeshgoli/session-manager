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
    // A reloaded page resumes the server's unfinished submission.
    attempt: CONFIG.unfinishedReview || null
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
    'label{display:flex;gap:6px;align-items:center}',
    // Touch: 16px text (no zoom on focus) and finger-sized controls.
    '.touch .sheet,.touch .sheet *{font-size:16px}',
    '.touch .sheet{width:100vw;border-radius:12px 12px 0 0;padding:14px}',
    '.touch .sheet button{padding:10px 16px}',
    '.touch textarea{min-height:9em}',
    '.touch .chip{font-size:16px;padding:10px 18px}'
  ].join('\n') }));
  var ui = el('div', { class: coarse ? 'touch' : '' });
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

  // The on-screen keyboard shrinks the visual viewport, not the layout one,
  // so a sheet pinned to the layout bottom would sit under the keyboard.
  // Pin it to the bottom of what is visible and cap it to that height.
  var vv = window.visualViewport;
  function placeSheet() {
    if (!vv) return;
    var hidden = Math.max(0, window.innerHeight - vv.height - vv.offsetTop);
    sheet.style.bottom = hidden + 'px';
    sheet.style.maxHeight = Math.max(160, vv.height - 12) + 'px';
  }
  if (vv) { vv.addEventListener('resize', placeSheet); vv.addEventListener('scroll', placeSheet); }

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
  var discardError = {};
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
      var parts = [el('span', { text: list.length + ' draft comment' + (list.length === 1 ? '' : 's') + ' on ' + sha7(sha) + (discardError[sha] ? ' (' + discardError[sha] + ')' : '') })];
      if (path) parts.push(el('a', { href: path, text: 'Submit them against ' + sha7(sha) }));
      parts.push(el('button', { class: 'lk', text: discardArmed[sha] ? 'Tap again to discard' : 'Discard', onclick: function () {
        if (!discardArmed[sha]) { discardArmed[sha] = 1; renderBanner(); return; }
        // Only drafts the server deleted leave the list; a failed delete
        // keeps its draft (and would still be submitted), so say so.
        var gone = {};
        var failures = 0;
        Promise.all(list.map(function (d) {
          return api('DELETE', '/drafts/' + d.id).then(function () { gone[d.id] = 1; }, function () { failures++; });
        })).then(function () {
          S.drafts = S.drafts.filter(function (d) { return !gone[d.id]; });
          delete discardArmed[sha];
          discardError[sha] = failures ? failures + ' could not be discarded; try again.' : '';
          markers(); renderBar(); renderBanner();
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

  // ---- inline bubbles ---------------------------------------------------
  // Like a GitHub review: the composer and each pending draft sit in the
  // page, right under the paragraph they are about. Being in the flow, the
  // composer scrolls into view above the on-screen keyboard. Each bubble is
  // a host element with its own shadow root, so the doc's styles and text
  // (textContent, selections) are untouched.
  var BUBBLE = 'sm-doc-bubble';
  var BUBBLE_CSS = [
    ':host{all:initial;display:block;margin:10px 0}',
    '[hidden]{display:none!important}',
    '*{box-sizing:border-box;font:' + (coarse ? '16px' : '14px') + '/1.45 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}',
    '.card{border:1px solid #d0d7de;border-left:4px solid #2563eb;border-radius:8px;background:#fff;color:#1f2328;padding:10px 12px;display:flex;flex-direction:column;gap:8px;box-shadow:0 1px 3px rgba(0,0,0,.12)}',
    '.tag{font-size:12px;font-weight:600;color:#9a6700;background:#fff8c5;border-radius:10px;padding:1px 8px;align-self:flex-start}',
    '.q{border-left:3px solid #d0d7de;padding-left:8px;color:#59636e;max-height:4.5em;overflow:hidden}',
    '.b{white-space:pre-wrap;word-break:break-word}',
    'textarea{width:100%;min-height:' + (coarse ? '7em' : '5em') + ';padding:8px;border:1px solid #d0d7de;border-radius:6px;resize:vertical;background:#fff;color:#1f2328}',
    '.row{display:flex;gap:8px;justify-content:flex-end;flex-wrap:wrap;align-items:center}',
    'button{cursor:pointer;border:1px solid #d0d7de;border-radius:6px;padding:' + (coarse ? '9px 16px' : '5px 12px') + ';background:#f6f8fa;color:#1f2328}',
    'button.p{background:#1f883d;border-color:#1f883d;color:#fff}button.d{color:#cf222e}button:disabled{opacity:.5}',
    '.err{color:#cf222e}.m{color:#59636e;font-size:13px}'
  ].join('\n');
  function blocksFor(line) { return document.querySelectorAll('[data-sm-line="' + line + '"]'); }
  function blockForDraft(d) {
    if (d.line == null) return null;
    var blocks = blocksFor(d.line);
    var probe = collapse(d.quote).slice(0, 40);
    for (var i = blocks.length - 1; i >= 0; i--) if (probe && collapse(blocks[i].textContent).indexOf(probe) >= 0) return blocks[i];
    return blocks[0] || null;
  }
  function inBubble(node) {
    var e = node && (node.nodeType === 1 ? node : node.parentElement);
    return !!(e && e.closest && e.closest(BUBBLE));
  }
  function bubble(block, kind) {
    var h = document.createElement(BUBBLE);
    h.setAttribute('data-kind', kind);
    h.style.cssText = 'all:initial;display:block;margin:10px 0';
    // A bubble can't sit between table rows: a row's goes in its last cell.
    var into = block.tagName === 'TR' ? (block.lastElementChild || block) : block;
    into.appendChild(h);
    var r = h.attachShadow ? h.attachShadow({ mode: 'open' }) : h;
    r.appendChild(el('style', { text: BUBBLE_CSS }));
    return { host: h, root: r };
  }
  function removeBubbles(kind) {
    Array.prototype.forEach.call(document.querySelectorAll(BUBBLE + '[data-kind="' + kind + '"]'), function (h) { h.remove(); });
  }
  // Keeps a bubble inside what is visible, below the bar and above the keyboard.
  function ensureVisible(h) {
    if (!h || !h.isConnected) return;
    var r = h.getBoundingClientRect();
    var top = collapsed ? 8 : bar.getBoundingClientRect().height + 8;
    var bottom = (vv ? vv.height + vv.offsetTop : window.innerHeight) - 8;
    if (r.bottom > bottom) window.scrollBy(0, Math.min(r.bottom - bottom, r.top - top));
    else if (r.top < top) window.scrollBy(0, r.top - top);
  }
  var composer = null; // {host, draftId, block}
  function keepComposerVisible() { if (composer) ensureVisible(composer.host); }
  window.addEventListener('resize', keepComposerVisible);
  if (vv) vv.addEventListener('resize', keepComposerVisible);

  var deleteArmed = null;
  function markers() {
    removeBubbles('draft');
    var byBlock = new Map();
    currentDrafts().forEach(function (d) {
      if (composer && composer.draftId === d.id) return;
      var b = blockForDraft(d);
      if (!b) return;
      if (!byBlock.has(b)) byBlock.set(b, []);
      byBlock.get(b).push(d);
    });
    byBlock.forEach(function (list, block) {
      var r = bubble(block, 'draft').root;
      list.forEach(function (d) {
        r.appendChild(el('div', { class: 'card' }, [
          el('span', { class: 'tag', text: 'Pending' }),
          el('div', { class: 'b', text: d.body }),
          el('div', { class: 'row' }, [
            el('button', { class: 'd', text: deleteArmed === d.id ? 'Tap again to delete' : 'Delete', onclick: function () {
              if (deleteArmed !== d.id) { deleteArmed = d.id; markers(); return; }
              deleteArmed = null;
              api('DELETE', '/drafts/' + d.id).then(function () {
                S.drafts = S.drafts.filter(function (x) { return x.id !== d.id; });
                markers(); renderBar();
              }, function (err) { alertBar(err.message); });
            } }),
            el('button', { text: 'Edit', onclick: function () { compose({ line: d.line, quote: d.quote, block: block }, d); } })
          ])
        ]));
      });
    });
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
    chip.onclick = function (e) {
      e.stopPropagation();
      var a2 = anchor;
      var keep = selected;
      chip.hidden = true; anchor = null; selected = null;
      compose(a2, null, keep);
    };
    placeChip();
  }
  function fromSelection() {
    var sel = window.getSelection && window.getSelection();
    if (!sel || sel.isCollapsed || !sel.rangeCount) return false;
    var range = sel.getRangeAt(0);
    var text = sel.toString().trim();
    if (!text || host.contains(range.commonAncestorContainer) || inBubble(range.commonAncestorContainer)) return false;
    unselect();
    var start = range.startContainer.nodeType === 1 ? range.startContainer : range.startContainer.parentElement;
    offer({ line: lineOf(range.startContainer), quote: text, block: start && start.closest ? start.closest(BLOCK) : null,
      rect: function () { return range.getBoundingClientRect(); } }, 'Comment');
    return true;
  }
  document.addEventListener('mouseup', function (e) {
    if (e.target === host || inBubble(e.target)) return;
    setTimeout(function () { if (!fromSelection() && !coarse) hideChip(); }, 0);
  });
  var selTimer = null;
  document.addEventListener('selectionchange', function () {
    if (!coarse) return;
    clearTimeout(selTimer);
    selTimer = setTimeout(fromSelection, 350);
  });
  document.addEventListener('click', function (e) {
    if (!coarse || e.target === host || inBubble(e.target)) return;
    var sel = window.getSelection && window.getSelection();
    if (sel && !sel.isCollapsed) return;
    var t = e.target;
    if (!t.closest || t.closest(INTERACTIVE)) return;
    var block = t.closest(BLOCK);
    if (!block || block === selected || (composer && composer.block === block)) { hideChip(); return; }
    unselect();
    selected = block;
    block.setAttribute('data-sm-selected', '');
    offer({
      line: parseInt(block.getAttribute('data-sm-line'), 10),
      quote: collapse(block.textContent).slice(0, 300),
      block: block,
      rect: function () { return block.getBoundingClientRect(); }
    }, 'Comment on this');
  });
  window.addEventListener('scroll', function () { if (!coarse && anchor) placeChip(); }, { passive: true });

  // ---- composer ----------------------------------------------------------
  function closeSheet() { sheet.hidden = true; clear(sheet); }
  function closeComposer() {
    if (!composer) return;
    composer.host.remove();
    if (composer.highlight) composer.highlight.removeAttribute('data-sm-selected');
    composer = null;
    markers();
  }
  function saveDraft(a, draft, body) {
    return draft
      ? api('PATCH', '/drafts/' + draft.id, { body: body }).then(function (d) { S.drafts = S.drafts.map(function (x) { return x.id === d.id ? d : x; }); })
      : api('POST', '/drafts', { sha: CONFIG.sha, line: a.line, quote: a.quote, body: body }).then(function (d) { S.drafts.push(d); });
  }
  // New comment (a = {line, quote, block}) or edit (draft). Inline under the
  // block when there is one; a comment with no line uses the sheet.
  function compose(a, draft, highlight) {
    closeComposer();
    closeSheet();
    var block = (a && a.block) || (draft && blockForDraft(draft));
    if (!block) return composeSheet(a, draft);
    var quote = draft ? draft.quote : a.quote;
    composer = { draftId: draft ? draft.id : null, block: block, highlight: highlight || null };
    markers();
    var b = bubble(block, 'composer');
    composer.host = b.host;
    var box = el('textarea', { placeholder: draft ? '' : 'Leave a comment' });
    if (draft) box.value = draft.body;
    var msg = el('div', { class: 'err' });
    var save = el('button', { class: 'p', text: draft ? 'Update comment' : 'Add draft', onclick: function () {
      var body = box.value.trim();
      if (!body) { msg.textContent = 'Write a comment first.'; return; }
      save.disabled = true;
      saveDraft(a, draft, body).then(function () { closeComposer(); renderBar(); })
        .catch(function (err) { save.disabled = false; msg.textContent = err.message; });
    } });
    b.root.appendChild(el('div', { class: 'card' }, [
      quote ? el('div', { class: 'q', text: quote.length > 160 ? quote.slice(0, 160) + '…' : quote }) : null,
      box,
      msg,
      el('div', { class: 'row' }, [el('button', { text: 'Cancel', onclick: closeComposer }), save])
    ]));
    box.focus({ preventScroll: true });
    ensureVisible(b.host);
    // Again once the keyboard has finished opening.
    setTimeout(keepComposerVisible, 350);
  }
  function composeSheet(a, draft) {
    clear(sheet);
    var box = el('textarea', { placeholder: 'Comment' });
    if (draft) box.value = draft.body;
    var msg = el('div', { class: 'err' });
    var save = el('button', { class: 'p', text: draft ? 'Save' : 'Add draft', onclick: function () {
      var body = box.value.trim();
      if (!body) { msg.textContent = 'Write a comment first.'; return; }
      save.disabled = true;
      saveDraft(a, draft, body).then(function () { closeSheet(); markers(); renderBar(); })
        .catch(function (err) { save.disabled = false; msg.textContent = err.message; });
    } });
    var quote = draft ? draft.quote : a.quote;
    // Buttons first: on a phone the keyboard takes the bottom of the screen.
    [
      el('h3', { text: draft ? 'Edit comment' : 'New comment' }),
      el('div', { class: 'row' }, [
        draft ? el('button', { class: 'd', text: 'Delete', onclick: function () {
          api('DELETE', '/drafts/' + draft.id).then(function () {
            S.drafts = S.drafts.filter(function (x) { return x.id !== draft.id; });
            closeSheet(); markers(); renderBar();
          }).catch(function (err) { msg.textContent = err.message; });
        } }) : null,
        el('button', { text: 'Cancel', onclick: closeSheet }),
        save
      ]),
      msg,
      quote ? el('div', { class: 'q', text: quote }) : null,
      el('div', { class: 'muted', text: 'Not tied to a line: posts as a comment on the file.' }),
      box
    ].forEach(function (c) { if (c) sheet.appendChild(c); });
    sheet.hidden = false;
    placeSheet();
    box.focus();
  }
  function alertBar(text) {
    banner.hidden = false;
    banner.appendChild(el('div', { class: 'err', text: text }));
  }

  // ---- review panel ------------------------------------------------------
  // The server posts every stored draft for this revision, so the panel
  // always shows the stored list: it reloads drafts when it opens and
  // checks them again right before submitting.
  function draftKey(list) {
    return list.map(function (d) { return d.id + '\u0000' + d.body; }).sort().join('\u0001');
  }
  function refreshDrafts() {
    return api('GET', '/drafts').then(function (res) {
      S.drafts = (res && res.drafts) || [];
      markers(); renderBar(); renderBanner();
    });
  }
  function openReview() {
    hideChip();
    refreshDrafts().then(function () { renderReview(''); }, function (err) {
      renderReview('Could not reload drafts: ' + err.message);
    });
  }
  function renderReview(notice) {
    clear(sheet);
    var drafts = currentDrafts();
    var shownKey = draftKey(drafts);
    sheet.appendChild(el('h3', { text: 'Review ' + sha7(CONFIG.sha) + ' (' + drafts.length + ' comment' + (drafts.length === 1 ? '' : 's') + ')' }));
    drafts.forEach(function (d) {
      sheet.appendChild(el('div', { class: 'item' }, [
        d.quote ? el('div', { class: 'q', text: d.quote.length > 160 ? d.quote.slice(0, 160) + '…' : d.quote }) : null,
        el('div', { text: d.body }),
        el('div', { class: 'row' }, [
          d.line != null ? el('button', { text: 'Show', onclick: function () { var b = blockForDraft(d); if (b) { closeSheet(); b.scrollIntoView({ block: 'center' }); } } }) : null,
          el('button', { text: 'Edit', onclick: function () { closeSheet(); compose(null, d); } })
        ])
      ]));
    });
    var msg = el('div', { class: notice ? 'err' : '', text: notice });
    if (!S.canComment) {
      sheet.appendChild(el('div', { class: 'ro', text: stateText()[0] + '. These drafts cannot be submitted.' }));
      sheet.appendChild(el('div', { class: 'row' }, [el('button', { text: 'Close', onclick: closeSheet })]));
      sheet.hidden = false;
      return;
    }
    // A retry reuses the earlier attempt's id, and the server keeps that
    // attempt's verdict and text, so the panel shows them and locks them.
    var attempt = S.attempt;
    var verdicts = [['approve', 'Approve'], ['changes_requested', 'Request changes'], ['comment', 'Comment']];
    var radios = verdicts.map(function (v) {
      var r = el('input', { type: 'radio', name: 'sm-verdict', value: v[0] });
      r.checked = v[0] === (attempt ? attempt.verdict : 'comment');
      r.disabled = !!attempt;
      return el('label', {}, [r, v[1]]);
    });
    var body = el('textarea', { placeholder: 'Overall comment (optional)' });
    if (attempt) { body.value = attempt.body; body.disabled = true; }
    var submit = el('button', { class: 'p', text: attempt ? 'Submit again' : 'Submit review', onclick: function () {
      var picked = root.querySelector ? root.querySelector('input[name=sm-verdict]:checked') : null;
      submit.disabled = true;
      msg.className = 'muted';
      msg.textContent = 'Submitting…';
      refreshDrafts().then(function () {
        if (draftKey(currentDrafts()) !== shownKey) {
          renderReview('The drafts changed on another device. Check them, then submit.');
          return;
        }
        if (!S.attempt) S.attempt = { id: newId(), verdict: picked ? picked.value : 'comment', body: body.value };
        return api('POST', '/review', { submission_id: S.attempt.id, sha: CONFIG.sha, verdict: S.attempt.verdict, body: S.attempt.body })
          .then(function (res) {
            S.attempt = null;
            S.drafts = S.drafts.filter(function (d) { return d.commit_sha !== CONFIG.sha; });
            markers(); renderBar();
            clear(sheet);
            sheet.appendChild(el('h3', { class: 'ok', text: 'Review posted' }));
            sheet.appendChild(el('a', { href: res.github_review_url, target: '_blank', rel: 'noopener', text: 'Open it on GitHub' }));
            sheet.appendChild(el('div', { class: 'row' }, [el('button', { text: 'Close', onclick: closeSheet })]));
          });
      }).catch(function (err) {
        // Keep the attempt: the server reconciles a retry against GitHub,
        // so resubmitting can never post the review twice.
        renderReview(err.message + ' Submitting again is safe.');
      });
    } });
    // Actions above the overall comment, clear of the on-screen keyboard.
    [el('div', { class: 'row' }, radios),
      el('div', { class: 'row' }, [el('button', { text: 'Cancel', onclick: closeSheet }), submit]),
      msg,
      attempt ? el('div', { class: 'muted', text: 'Retrying your earlier submission with its verdict and text.' }) : null,
      body
    ].forEach(function (c) { if (c) sheet.appendChild(c); });
    sheet.hidden = false;
    placeSheet();
  }

  document.addEventListener('keydown', function (e) { if (e.key === 'Escape') { closeSheet(); closeComposer(); hideChip(); } });
  renderBar();
  renderBanner();
  markers();
  pollHead();
  setInterval(pollHead, 60000);
  document.addEventListener('visibilitychange', pollHead);
})
