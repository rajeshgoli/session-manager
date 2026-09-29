// Web watch refresh (sm#1452). Refetches /watch/state while the tab is
// visible and re-renders the cards with the markup of watch.rs
// render_sessions, keeping open cards open. On a failed fetch the last
// render stays and the top bar shows "stale · <age>".
(function () {
  var W = document.getElementById('w'), S = document.getElementById('ws');
  if (!W || !window.fetch) return;
  var every = Math.max(2, +W.getAttribute('data-refresh') || 3) * 1000;
  var open = {}, last = null, counts = S ? S.textContent : '', okAt = Date.now(), stale = false, busy = false;
  var editor = null, editorId = null, refreshAgain = false;
  var defaultsPanel = document.getElementById('handoff-defaults');

  function e(x) {
    return String(x == null ? '' : x).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }
  function ms(x) {
    var t = Date.parse(String(x || '').replace(/(\.\d{3})\d+/, '$1'));
    return isNaN(t) ? null : t;
  }
  function span(n) {
    return n < 60 ? n + 's' : n < 3600 ? Math.floor(n / 60) + 'm'
      : n < 86400 ? Math.floor(n / 3600) + 'h' : Math.floor(n / 86400) + 'd';
  }
  function age(x, now) {
    var t = ms(x);
    return t == null ? '?' : span(Math.max(0, Math.floor((now - t) / 1000)));
  }
  function arr(x) { return Array.isArray(x) ? x : []; }
  function chip(state) {
    var tone = state === 'open' ? 'c' : state === 'merged' || state === 'closed' ? 'g' : '';
    return '<span class="chip ' + tone + '">' + e(state) + '</span>';
  }
  function ext(href, label) {
    return /^https:\/\//.test(href || '')
      ? '<a class="mt lk" href="' + e(href) + '">' + e(label) + ' ↗</a>'
      : '<span class="mt">' + e(label) + '</span>';
  }
  function repoName(r) { r = String(r || ''); return r.slice(r.lastIndexOf('/') + 1); }
  function sections(rows) {
    var b = '';
    rows.forEach(function (r) {
      if (r[1]) b += '<span class="lbl">' + r[0] + '</span><span>' + r[1] + '</span>';
    });
    return b ? '<div class="sec">' + b + '</div>' : '';
  }

  function work(v) {
    var hist = arr(v.review_history), parts = [], trees = [];
    arr(v.claims).forEach(function (c) {
      var n = c.number || 0;
      if (c.kind === 'pr') {
        var h = hist.filter(function (h) {
          return String(h.repo || '').toLowerCase() === String(c.repo || '').toLowerCase() && h.pr_number === n;
        })[0];
        var k = h && h.request_count || 0;
        parts.push(ext(c.url, 'PR #' + n) + ' ' + chip(c.state || '') +
          (k > 0 ? ' <span class="m">' + k + ' Codex</span>' : ''));
      } else {
        parts.push('<a class="mt lk" href="' + e(c.history_path) + '">ticket #' + n + '</a> ' + chip(c.state || ''));
      }
      if (c.worktree_path && trees.indexOf(c.worktree_path) < 0) trees.push(c.worktree_path);
    });
    trees.forEach(function (w) { parts.push('<span class="m">worktree ' + e(w) + '</span>'); });
    return parts.join(' <span class="m">·</span> ');
  }
  function docs(list, now) {
    return list.map(function (d) {
      return '<a class="lk" href="' + e(d.reader_path) + '">' + e(d.title) + '</a> <span class="chip ' +
        (d.state === 'review_requested' ? 'a' : 'v') + '">' + e(String(d.state || '').replace(/_/g, ' ')) +
        '</span> <span class="m">' + age(d.published_at, now) + '</span>' +
        (d.review_undelivered === true ? ' <span class="chip r">review not delivered</span>' : '');
    }).join('<br>');
  }
  function reviews(v, waiting, now) {
    var jobs = arr(v.jobs);
    var lines = waiting.filter(function (i) {
      return !(i.kind === 'queue_job' && jobs.some(function (j) { return j.id === i.id; }));
    }).map(function (i) {
      return '<span class="' + (i.kind === 'owner_review' ? 'amb' : '') + '">' + e(i.label) +
        '</span> <span class="m">waiting ' + age(i.since, now) + '</span>';
    });
    arr(v.review_history).forEach(function (h) {
      lines.push('<span class="mt">' + e(repoName(h.repo)) + '#' + (h.pr_number || 0) +
        '</span> <span class="m">' + (h.landed_count || 0) + ' landed · ' + (h.request_count || 0) + ' requested</span>');
    });
    return lines.join('<br>');
  }
  function jobs(v, now) {
    return arr(v.jobs).map(function (j) {
      return '<span class="mt">[' + e(j.type) + '] ' + e(j.label) + '</span> <span class="m">' + e(j.state) + ' ' +
        age(j.state === 'running' ? j.started_at : j.queued_at, now) + '</span>';
    }).join('<br>');
  }
  function card(v, now) {
    var waiting = arr(v.waiting_on), claims = arr(v.claims), ds = arr(v.docs), chips = '';
    var owner = waiting.some(function (i) { return i.kind === 'owner_review'; });
    var hit = v.collision === true;
    var lead = claims.filter(function (c) { return c.kind === 'ticket'; })[0] || claims[0];
    if (lead) {
      chips += ' <span class="chip c">' + (lead.kind === 'pr' ? 'PR ' : '') + '#' + (lead.number || 0) +
        (claims.length > 1 ? ' +' + (claims.length - 1) : '') + '</span>';
    }
    if (ds.length) {
      var unread = ds.filter(function (d) { return /^(new|updated|review_requested)$/.test(d.state); }).length;
      var note = ds.some(function (d) { return d.state === 'review_requested'; }) ? ' · review requested'
        : unread > 0 ? ' · ' + unread + ' new' : '';
      chips += ' <span class="chip v">docs ' + ds.length + note + '</span>';
    }
    if (owner) chips += ' <span class="chip a">waiting on you</span>';
    if (hit) chips += ' <span class="chip r">2 agents</span>';
    var att = v.attach ? '<code class="cp mt" data-cp="' + e(v.attach) + '" title="Click to copy">$ ' + e(v.attach) + ' ⧉</code>' : '';
    return '<details class="card ' + (hit ? 'r' : owner ? 'a' : 'c') + '" data-id="' + e(v.id) +
      '" style="margin-left:' + Math.min(v.depth || 0, 6) * 14 + 'px"><summary><span class="row"><span class="dot ' +
      e(v.state) + '"></span><span class="mt nm">' + e(v.name) + '</span><span class="m">' + e(v.provider) + ' · ' +
      e(v.state) + ' ' + age(v.last_activity, now) + '</span>' + chips + '</span>' +
      (v.status_text ? '<span class="st">' + e(v.status_text) + '</span>' : '') + handoffLine(v) + '</summary>' +
      sections([['Work', work(v)], ['Docs', docs(ds, now)], ['Reviews', reviews(v, waiting, now)],
        ['Jobs', jobs(v, now)], ['Attach', att]]) + '</details>';
  }
  function render(doc) {
    var now = ms(doc.generated_at) || 0, list = arr(doc.sessions);
    if (!list.length) return '<p class="dim">No live sessions.</p>';
    return list.map(function (v) {
      return (v.group != null ? '<div class="grp">' + e(v.group) + '</div>' : '') + card(v, now);
    }).join('');
  }
  window.smWatchRender = render;

  function handoffText(v) {
    return (typeof v.context_percent === 'number' ? 'ctx ' + Math.round(v.context_percent) + '% · ' : '') + (v.handoff.display || '');
  }
  function handoffLine(v) {
    return v.handoff ? '<span class="st"><button type="button" data-handoff="' + e(v.id) + '">' + e(handoffText(v)) + '</button></span>' : '';
  }
  function api(path, method, body) {
    var options = { method: method, credentials: 'same-origin', cache: 'no-store' };
    if (body) { options.headers = { 'Content-Type': 'application/json' }; options.body = JSON.stringify(body); }
    return fetch(path, options).then(function (r) {
      return r.json().then(function (value) {
        if (!r.ok) throw new Error(value.detail || 'HTTP ' + r.status);
        return value;
      });
    });
  }
  function closeEditor() {
    if (editor) editor.remove();
    editor = null; editorId = null; last = null; refresh();
  }
  function notice(panel, text) { panel.querySelector('[role="status"]').textContent = text; }
  function saving(panel, enabled) {
    panel.querySelectorAll('input, button').forEach(function (control) { control.disabled = enabled; });
  }
  function write(panel, path, body) {
    saving(panel, true); notice(panel, 'Saving…');
    return api(path, 'PUT', body).then(function (value) {
      notice(panel, 'Saved'); refresh(); return value;
    }).catch(function (error) { notice(panel, error.message); throw error; })
      .finally(function () { saving(panel, false); });
  }
  function percent(input, zero) {
    var value = input.value.trim(), number = Number(value);
    if (!value || !Number.isFinite(number) || number > 100 || (zero ? number < 0 : number <= 0)) {
      throw new Error(zero ? 'Enter a percentage from 0 to 100' : 'Enter a percentage greater than 0 and at most 100');
    }
    return number;
  }
  function openHandoff(button) {
    if (editor) closeEditor();
    var id = button.getAttribute('data-handoff'), panel = document.createElement('div');
    var path = '/sessions/' + encodeURIComponent(id) + '/handoff-policy';
    panel.className = 'handoff-panel';
    panel.innerHTML = '<strong>Context handoff</strong> <button type="button" data-close>Close</button>' +
      '<p role="status">Loading…</p><div data-controls hidden>' +
      '<label><input type="checkbox" data-enabled> Enabled</label> ' +
      '<label>Threshold (%) <input type="number" min="1" max="100" step="1" data-threshold></label> ' +
      '<button type="button" data-default>Use default</button> <button type="button" data-now>Hand off now</button>' +
      '<span data-confirm hidden> Ask this agent to hand off? <button type="button" data-yes>Confirm handoff</button> <button type="button" data-no>Cancel</button></span></div>';
    button.closest('details').open = true; open[id] = true;
    button.closest('details').appendChild(panel); editor = panel; editorId = id;
    panel.querySelector('[data-close]').onclick = closeEditor;
    var enabled = panel.querySelector('[data-enabled]'), threshold = panel.querySelector('[data-threshold]');
    function show(value) {
      enabled.checked = value.enabled; threshold.value = value.threshold_percent;
      panel.querySelector('[data-controls]').hidden = false;
    }
    api(path, 'GET').then(function (value) { show(value); notice(panel, 'Using ' + value.source + ' policy'); })
      .catch(function (error) { notice(panel, error.message); });
    function update(body) { write(panel, path, body).then(show).catch(function () {}); }
    enabled.onchange = function () { update({ enabled: enabled.checked }); };
    threshold.onchange = function () {
      try {
        var number = percent(threshold, false);
        if (!Number.isInteger(number)) throw new Error('Enter an integer from 1 to 100');
        update({ threshold_percent: number });
      } catch (error) { notice(panel, error.message); }
    };
    panel.querySelector('[data-default]').onclick = function () { update({ use_default: true }); };
    var confirm = panel.querySelector('[data-confirm]');
    panel.querySelector('[data-now]').onclick = function () { confirm.hidden = false; };
    panel.querySelector('[data-no]').onclick = function () { confirm.hidden = true; };
    panel.querySelector('[data-yes]').onclick = function () { confirm.hidden = true; update({ ask_now: true }); };
  }
  function renderDefaults(value) {
    var providers = Object.keys(value.providers || {});
    ['claude', 'codex-fork', 'codex-app'].forEach(function (p) { if (providers.indexOf(p) < 0) providers.push(p); });
    var fields = providers.sort().map(function (p) { return ['providers.' + p, 'Enable ' + p, !!value.providers[p]]; });
    [['threshold_percent', 'Context threshold (%)'], ['ask_on_codex_review', 'Ask on Codex review request'],
      ['ask_on_doc_review', 'Ask on doc review request'], ['review_floor_percent', 'Review floor (%)'],
      ['reminder_percent', 'Reminder at (%)']].forEach(function (f) { fields.push([f[0], f[1], value[f[0]]]); });
    defaultsPanel.innerHTML = '<strong>Handoff defaults</strong> <button type="button" data-close>Close</button><p role="status"></p>' +
      fields.map(function (f) {
        var bool = typeof f[2] === 'boolean';
        return '<p><label>' + e(f[1]) + ' <input data-field="' + e(f[0]) + '" type="' + (bool ? 'checkbox' : 'number') + '" ' +
          (bool ? (f[2] ? 'checked' : '') : 'min="0" max="100" step="any" value="' + e(f[2]) + '"') + '></label></p>';
      }).join('');
    defaultsPanel.querySelector('[data-close]').onclick = function () { defaultsPanel.hidden = true; };
    defaultsPanel.querySelectorAll('[data-field]').forEach(function (input) {
      input.onchange = function () {
        try {
          var field = input.getAttribute('data-field'), body = {};
          var next = input.type === 'checkbox' ? input.checked : percent(input, field === 'review_floor_percent');
          if (field.indexOf('providers.') === 0) { body.providers = {}; body.providers[field.slice(10)] = next; }
          else body[field] = next;
          write(defaultsPanel, '/handoff-defaults', body).then(renderDefaults).catch(function () {});
        } catch (error) { notice(defaultsPanel, error.message); }
      };
    });
  }
  var defaultsButton = document.getElementById('handoff-defaults-open');
  if (defaultsButton && defaultsPanel) defaultsButton.onclick = function () {
    defaultsPanel.hidden = false;
    defaultsPanel.innerHTML = '<p role="status">Loading…</p>';
    api('/handoff-defaults', 'GET').then(renderDefaults).catch(function (error) { notice(defaultsPanel, error.message); });
  };

  function label() {
    if (!S) return;
    var n = Math.floor((Date.now() - okAt) / 1000);
    S.textContent = stale ? 'stale · ' + span(n) : counts + ' · ' + span(n) + ' ago';
    S.className = stale ? 'm stale' : 'm';
  }
  function refresh() {
    if (busy) { refreshAgain = true; return; }
    if (document.visibilityState === 'hidden') return;
    busy = true;
    fetch('/watch/state' + location.search, { credentials: 'same-origin', cache: 'no-store' })
      .then(function (r) { if (!r.ok) throw new Error(r.status); return r.json(); })
      .then(function (doc) {
        var html = render(doc);
        if (html !== last) {
          // Retain the actual form (including draft input and handlers), while
          // replacing all dashboard data. Restore focus after reattaching it.
          var focused = editor && editor.contains(document.activeElement) ? document.activeElement : null;
          var retained = editor;
          last = html;
          W.innerHTML = html;
          W.querySelectorAll('details[data-id]').forEach(function (d) {
            if (open[d.getAttribute('data-id')]) d.open = true;
            if (retained && d.getAttribute('data-id') === editorId) {
              d.appendChild(retained); retained = null;
              if (focused) focused.focus({ preventScroll: true });
            }
          });
          // An ended or filtered-out agent must not leave an orphan editor.
          if (retained) { editor = null; editorId = null; }
        }
        var c = doc.counts || {};
        counts = (c.live || 0) + ' live' + (c.waiting_on_owner ? ' · ' + c.waiting_on_owner + ' waiting on you' : '');
        okAt = Date.now(); stale = false; label();
      })
      .catch(function () { stale = true; label(); })
      .then(function () { busy = false; if (refreshAgain) { refreshAgain = false; refresh(); } });
  }

  W.addEventListener('toggle', function (ev) {
    var id = ev.target.getAttribute && ev.target.getAttribute('data-id');
    if (id) open[id] = ev.target.open;
  }, true);
  W.addEventListener('click', function (ev) {
    var handoff = ev.target.closest && ev.target.closest('[data-handoff]');
    if (handoff) { ev.preventDefault(); openHandoff(handoff); return; }
    var c = ev.target.closest && ev.target.closest('.cp');
    if (!c || !navigator.clipboard) return;
    navigator.clipboard.writeText(c.getAttribute('data-cp')).then(function () {
      c.classList.add('ok');
      setTimeout(function () { c.classList.remove('ok'); }, 1200);
    });
  });
  document.addEventListener('visibilitychange', function () {
    if (document.visibilityState === 'visible') refresh();
  });
  setInterval(refresh, every);
  setInterval(label, 1000);
  label();
})();
