(function (CONFIG) {
  'use strict';
  // sm Inbox pages (sm#1647): Done on the list, and quote, send and Done on
  // an agent thread. The pages are painted on the server; this only posts.
  function api(path, body) {
    var headers = { Accept: 'application/json', 'Content-Type': 'application/json' };
    if (CONFIG.token) headers['X-SM-Doc-Token'] = CONFIG.token;
    return fetch(path, {
      method: 'POST', headers: headers, credentials: 'same-origin', body: JSON.stringify(body)
    }).then(function (r) {
      return r.text().then(function (t) {
        var j = null;
        try { j = t ? JSON.parse(t) : null; } catch (e) { j = null; }
        if (!r.ok) throw new Error((j && j.detail) || ('HTTP ' + r.status));
        return j;
      });
    });
  }
  function newId() {
    if (window.crypto && crypto.randomUUID) { try { return crypto.randomUUID(); } catch (e) { /* insecure context */ } }
    var s = '';
    for (var i = 0; i < 32; i++) s += Math.floor(Math.random() * 16).toString(16);
    return s;
  }
  function collapse(text) { return (text || '').replace(/\s+/g, ' ').trim(); }

  if (CONFIG.page === 'list') {
    Array.prototype.forEach.call(document.querySelectorAll('button.done'), function (b) {
      b.addEventListener('click', function () {
        b.disabled = true;
        api('/inbox/done', { thread_key: b.getAttribute('data-key') }).then(function () {
          location.reload();
        }, function (err) { b.disabled = false; b.textContent = err.message; });
      });
    });
    Array.prototype.forEach.call(document.querySelectorAll('button.archive,button.unarchive'), function (b) {
      b.addEventListener('click', function () {
        b.disabled = true;
        var method = b.classList.contains('unarchive') ? 'unarchive' : 'archive';
        api('/inbox/' + method, { thread_key: b.getAttribute('data-key') }).then(function () {
          location.reload();
        }, function (err) { b.disabled = false; b.textContent = err.message; });
      });
    });
    var fold = document.getElementById('folded');
    if (fold) {
      fold.open = localStorage.getItem('sm-inbox-folded-open') === 'true';
      fold.addEventListener('toggle', function () { localStorage.setItem('sm-inbox-folded-open', String(fold.open)); });
    }
    return;
  }

  var msg = document.getElementById('msg');
  var box = document.getElementById('box');
  var send = document.getElementById('send');
  var done = document.getElementById('done');
  var archive = document.getElementById('archive');
  var replyTarget = document.getElementById('reply-target');
  var qs = document.getElementById('qs');
  var quotes = [];
  // One id per attempt: a retry after a lost response is not sent twice.
  var attempt = null;

  function say(text, err) { if (msg) { msg.textContent = text; msg.className = err ? 'msg err' : 'msg'; } }

  function renderQuotes() {
    if (!qs) return;
    while (qs.firstChild) qs.removeChild(qs.firstChild);
    quotes.forEach(function (q, i) {
      var row = document.createElement('div');
      row.className = 'qc';
      var span = document.createElement('span');
      span.textContent = q.quote;
      var x = document.createElement('button');
      x.textContent = '×';
      x.title = 'Remove quote';
      x.addEventListener('click', function () {
        if (q.el) q.el.classList.remove('quoted');
        quotes.splice(i, 1);
        attempt = null;
        renderQuotes();
      });
      row.appendChild(span);
      row.appendChild(x);
      qs.appendChild(row);
    });
  }

  if (CONFIG.canSend) {
    document.addEventListener('click', function (e) {
      var t = e.target;
      if (!t || !t.closest || t.closest('a,button,input,textarea')) return;
      var md = t.closest('.md[data-msg]');
      var block = t.closest('[data-sm-line]');
      if (!md || !block || !md.contains(block)) return;
      if (window.getSelection && String(window.getSelection()).length) return;
      for (var i = 0; i < quotes.length; i++) if (quotes[i].el === block) return;
      var text = collapse(block.textContent);
      if (!text) return;
      quotes.push({ message_id: md.getAttribute('data-msg'), quote: text.slice(0, 2000), el: block });
      block.classList.add('quoted');
      attempt = null;
      renderQuotes();
      if (box) box.focus();
    });
    if (box) box.addEventListener('input', function () { attempt = null; });
    if (send) send.addEventListener('click', function () {
      var body = box ? box.value : '';
      if (!body.trim() && !quotes.length) { say('Write something or tap a paragraph to quote it.', true); return; }
      attempt = attempt || newId();
      send.disabled = true;
      say('Sending…');
      api('/inbox/thread/' + encodeURIComponent(CONFIG.threadKey) + '/send', {
        submission_id: attempt,
        to: replyTarget ? replyTarget.value : CONFIG.sessionId,
        body: body,
        quotes: quotes.map(function (q) { return { message_id: q.message_id, quote: q.quote }; })
      }).then(function (res) {
        say('Sent to ' + (res.delivered_to_session_name || 'the agent') + '.');
        location.replace(location.pathname + '?bottom=1');
      }, function (err) { send.disabled = false; say(err.message, true); });
    });
  }

  // Assign to agent on an sm alert (sm#2414): one sheet at a time opens
  // under its alert; Start hands the alert to a new agent and the button
  // becomes the agent it went to.
  var sheetTemplate = document.getElementById('assign-sheet');
  var openSheet = null;
  function closeSheet() {
    if (!openSheet) return;
    openSheet.button.hidden = false;
    openSheet.el.remove();
    openSheet = null;
  }
  function ownerPill(res) {
    var a = document.createElement('a');
    a.className = 'owner';
    a.href = res.thread_path;
    var dot = document.createElement('span');
    dot.className = 'dot';
    var chev = document.createElement('span');
    chev.className = 'chev';
    chev.textContent = '›';
    a.appendChild(dot);
    a.appendChild(document.createTextNode((res.name || 'An agent') + ' is on it'));
    a.appendChild(chev);
    return a;
  }
  if (sheetTemplate) Array.prototype.forEach.call(document.querySelectorAll('button.assign'), function (b) {
    b.addEventListener('click', function () {
      closeSheet();
      var el = sheetTemplate.content.firstElementChild.cloneNode(true);
      var start = el.querySelector('button.start');
      var cancel = el.querySelector('button.cancel');
      var note = el.querySelector('.msg');
      openSheet = { el: el, button: b };
      b.hidden = true;
      b.closest('.b').appendChild(el);
      el.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
      start.focus();
      cancel.addEventListener('click', closeSheet);
      el.addEventListener('keydown', function (e) { if (e.key === 'Escape') closeSheet(); });
      start.addEventListener('click', function () {
        var chosen = el.querySelector('input[name="agent-type"]:checked');
        start.disabled = true;
        cancel.disabled = true;
        start.textContent = 'Starting…';
        note.textContent = '';
        api('/inbox/alerts/' + encodeURIComponent(b.getAttribute('data-assign')) + '/assign',
          chosen ? { agent_type: chosen.value } : {}).then(function (res) {
          openSheet = null;
          el.remove();
          // As the server paints it: the time alone, then who has it.
          var meta = b.parentNode.querySelector('.m');
          if (meta) meta.textContent = meta.textContent.split(' · ')[0];
          b.replaceWith(ownerPill(res));
        }, function (err) {
          start.disabled = false;
          cancel.disabled = false;
          start.textContent = 'Start agent';
          note.textContent = err.message;
        });
      });
    });
  });

  if (done) done.addEventListener('click', function () {
    done.disabled = true;
    api('/inbox/done', { thread_key: CONFIG.threadKey }).then(function () {
      say('Done. This thread left your Inbox.');
    }, function (err) { done.disabled = false; say(err.message, true); });
  });
  if (archive) archive.addEventListener('click', function () {
    archive.disabled = true;
    api('/inbox/archive', { thread_key: CONFIG.threadKey }).then(function () {
      location.replace('/inbox');
    }, function (err) { archive.disabled = false; say(err.message, true); });
  });
  document.addEventListener('keydown', function (e) {
    if (e.key !== 'y' || !archive || e.target.closest('input,textarea,[contenteditable]')) return;
    archive.click();
  });

  var target = CONFIG.at && document.getElementById(CONFIG.at);
  if (target) {
    target.classList.add('hl');
    target.scrollIntoView({ block: 'center' });
  } else {
    window.scrollTo(0, document.body.scrollHeight);
  }
})
