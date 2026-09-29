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
    return;
  }

  var msg = document.getElementById('msg');
  var box = document.getElementById('box');
  var send = document.getElementById('send');
  var done = document.getElementById('done');
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
      api('/inbox/agent/' + encodeURIComponent(CONFIG.sessionId) + '/send', {
        submission_id: attempt,
        body: body,
        quotes: quotes.map(function (q) { return { message_id: q.message_id, quote: q.quote }; })
      }).then(function (res) {
        say('Sent to ' + (res.delivered_to_session_name || 'the agent') + '.');
        location.replace(location.pathname + '?bottom=1');
      }, function (err) { send.disabled = false; say(err.message, true); });
    });
  }

  if (done) done.addEventListener('click', function () {
    done.disabled = true;
    api('/inbox/done', { thread_key: CONFIG.threadKey }).then(function () {
      say('Done. This thread left your Inbox.');
    }, function (err) { done.disabled = false; say(err.message, true); });
  });

  var target = CONFIG.at && document.getElementById(CONFIG.at);
  if (target) {
    target.classList.add('hl');
    target.scrollIntoView({ block: 'center' });
  } else {
    window.scrollTo(0, document.body.scrollHeight);
  }
})
