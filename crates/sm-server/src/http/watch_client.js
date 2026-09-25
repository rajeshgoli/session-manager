// Web watch refresh (sm#1452). Refetches /watch/state while the tab is
// visible and re-renders the cards with the markup of watch.rs
// render_sessions, keeping open cards open. On a failed fetch the last
// render stays and the top bar shows "stale · <age>".
(function () {
  var W = document.getElementById('w'), S = document.getElementById('ws');
  if (!W || !window.fetch) return;
  var every = Math.max(2, +W.getAttribute('data-refresh') || 3) * 1000;
  var open = {}, last = null, counts = S ? S.textContent : '', okAt = Date.now(), stale = false, busy = false;

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
      (v.status_text ? '<span class="st">' + e(v.status_text) + '</span>' : '') + '</summary>' +
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

  function label() {
    if (!S) return;
    var n = Math.floor((Date.now() - okAt) / 1000);
    S.textContent = stale ? 'stale · ' + span(n) : counts + ' · ' + span(n) + ' ago';
    S.className = stale ? 'm stale' : 'm';
  }
  function refresh() {
    if (busy || document.visibilityState === 'hidden') return;
    busy = true;
    fetch('/watch/state' + location.search, { credentials: 'same-origin', cache: 'no-store' })
      .then(function (r) { if (!r.ok) throw new Error(r.status); return r.json(); })
      .then(function (doc) {
        var html = render(doc);
        if (html !== last) {
          last = html;
          W.innerHTML = html;
          W.querySelectorAll('details[data-id]').forEach(function (d) {
            if (open[d.getAttribute('data-id')]) d.open = true;
          });
        }
        var c = doc.counts || {};
        counts = (c.live || 0) + ' live' + (c.waiting_on_owner ? ' · ' + c.waiting_on_owner + ' waiting on you' : '');
        okAt = Date.now(); stale = false; label();
      })
      .catch(function () { stale = true; label(); })
      .then(function () { busy = false; });
  }

  W.addEventListener('toggle', function (ev) {
    var id = ev.target.getAttribute && ev.target.getAttribute('data-id');
    if (id) open[id] = ev.target.open;
  }, true);
  W.addEventListener('click', function (ev) {
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
