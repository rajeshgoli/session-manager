(() => {
  'use strict';
  const root = document.querySelector('#board');
  const message = document.querySelector('#board-message');
  const dialog = document.querySelector('#board-start');
  const form = document.querySelector('#board-start-form');
  const field = name => form.elements.namedItem(name);
  const error = document.querySelector('#board-start-error');
  const submit = document.querySelector('#board-submit');
  let board, selected, checkout, busy = false, modelGeneration = 0, selectionGeneration = 0;
  const folds = new Map();
  async function api(path, method = 'GET', data) {
    const response = await fetch(path, {method, credentials:'same-origin', headers:{'Content-Type':'application/json'}, body:data === undefined ? undefined : JSON.stringify(data)});
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body.detail || `Request failed (${response.status})`);
    return body;
  }
  function restore() {
    root.querySelectorAll('details').forEach(el => {
      const key = el.dataset.lane ? `lane-${el.dataset.lane}` : el.dataset.fold;
      if (!key) return;
      let saved = folds.get(key);
      try { if (saved === undefined) saved = localStorage.getItem(`sm-board-${key}`); } catch (_) {}
      if (saved !== undefined && saved !== null) el.open = saved === 'true';
      if (el.id && location.hash === `#${el.id}`) el.open = true;
      el.addEventListener('toggle', () => {
        folds.set(key, String(el.open));
        try { localStorage.setItem(`sm-board-${key}`, String(el.open)); } catch (_) {}
      });
    });
    root.querySelectorAll('[data-age]').forEach(el => {
      const date = Date.parse(el.dataset.age);
      if (!Number.isFinite(date)) return;
      const seconds = Math.max(0, Math.floor((Date.now()-date)/1000));
      el.textContent = seconds < 60 ? `${seconds}s ago` : seconds < 3600 ? `${Math.floor(seconds/60)}m ago` : `${Math.floor(seconds/3600)}h ago`;
    });
  }
  async function refresh() {
    if (busy || document.hidden) return;
    busy = true;
    try {
      board = await api('/client/board?html=true');
      const draft = root.querySelector('#board-add');
      const draftValues = draft ? [...new FormData(draft)] : [];
      root.innerHTML = board.html;
      const restored = root.querySelector('#board-add');
      if (restored) for (const [name, value] of draftValues) restored.elements.namedItem(name).value = value;
      restore();
      if (!document.hidden) {
        await api('/client/board/seen', 'POST');
        window.dispatchEvent(new Event('sm-board-seen'));
      }
    } catch (e) { message.textContent = e.message; }
    finally { busy = false; }
  }
  async function models() {
    const generation = ++modelGeneration;
    submit.disabled = true;
    error.textContent = '';
    const provider = field('provider').value;
    field('model').replaceChildren();
    field('reasoning_effort').replaceChildren(...(provider === 'claude' ? ['low','medium','high','max'] : ['low','medium','high','xhigh']).map(v => new Option(v,v)));
    const effort = board.start_defaults.reasoning_effort || 'high';
    if ([...field('reasoning_effort').options].some(o => o.value === effort)) field('reasoning_effort').value = effort;
    try {
      const result = await api(`/client/session-models?provider=${encodeURIComponent(provider)}&working_dir=${encodeURIComponent(checkout)}`);
      if (generation !== modelGeneration) return;
      field('model').replaceChildren(...result.models.map(m => new Option(m,m)));
      const desired = board.start_defaults.model;
      const available = result.models.includes(desired);
      if (available) field('model').value = desired;
      document.querySelector('#board-model-note').textContent = available ? '' : 'The configured default model is unavailable. The first available model is selected.';
      if (!result.models.length) throw new Error('No models available for this checkout.');
      submit.disabled = false;
    } catch (e) { if (generation === modelGeneration) error.textContent = e.message; }
  }
  field('provider').addEventListener('change', models);
  document.querySelector('#board-cancel').onclick = () => dialog.close();
  dialog.addEventListener('close', () => { ++selectionGeneration; ++modelGeneration; });
  root.addEventListener('click', async event => {
    const button = event.target.closest('button');
    if (!button || button.type === 'submit') return;
    try {
      if (button.hasAttribute('data-start')) {
        const generation = ++selectionGeneration;
        selected = {repo:button.dataset.repo, number:Number(button.dataset.start)};
        document.querySelector('#board-start-title').textContent = `Start ${selected.repo}#${selected.number}`;
        error.textContent = '';
        submit.disabled = true;
        dialog.showModal();
        const options = await api(`/client/board/start-options?repo=${encodeURIComponent(selected.repo)}&number=${selected.number}`);
        if (generation !== selectionGeneration || !dialog.open) return;
        checkout = options.working_dir;
        field('name').value = options.name;
        field('brief').value = options.brief;
        if (!board) board = await api('/client/board');
        if (generation !== selectionGeneration || !dialog.open) return;
        field('provider').value = board.start_defaults.provider || 'claude';
        await models();
        return;
      }
      if (button.hasAttribute('data-end') || button.hasAttribute('data-end-no')) {
        const id = button.dataset.end || button.dataset.endNo;
        root.querySelector(`[data-confirm="${id}"]`).hidden = !button.hasAttribute('data-end');
        return;
      }
      button.disabled = true;
      if (button.hasAttribute('data-refresh')) {
        await api('/client/board/refresh','POST');
        // The pass is asynchronous; show its result shortly after the request too.
        setTimeout(refresh, 2000);
      } else if (button.hasAttribute('data-move')) {
        const ids = [...root.querySelectorAll('[data-lane]')].map(el => Number(el.dataset.lane));
        const from = ids.indexOf(Number(button.dataset.id)), to = from + Number(button.dataset.move);
        if (from < 0 || to < 0 || to >= ids.length) return;
        [ids[from],ids[to]] = [ids[to],ids[from]];
        await api('/client/board/order','PUT',{lane_ids:ids});
      } else if (button.hasAttribute('data-end-yes')) {
        await api(`/client/board/lanes/${button.dataset.endYes}`,'DELETE');
      }
      await refresh();
    } catch (e) { (dialog.open ? error : message).textContent = e.message; }
    finally { if (button.isConnected) button.disabled = false; }
  });
  root.addEventListener('submit', async event => {
    if (event.target.id !== 'board-add') return;
    event.preventDefault();
    const values = new FormData(event.target);
    const button = event.target.querySelector('button');
    if (button.disabled) return;
    button.disabled = true;
    try { await api('/client/board/lanes','POST',{repo:values.get('repo'),number:Number(values.get('number'))}); await refresh(); }
    catch (e) { message.textContent = e.message; }
    finally { button.disabled = false; }
  });
  form.addEventListener('submit', async event => {
    event.preventDefault();
    if (submit.disabled) return;
    submit.disabled = true;
    error.textContent = '';
    try {
      const result = await api('/client/board/start','POST',{...selected,...Object.fromEntries(new FormData(form))});
      dialog.close();
      message.textContent = `Started ${result.name}`;
      await refresh();
    } catch (e) { error.textContent = e.message; }
    finally { submit.disabled = false; }
  });
  restore();
  refresh();
  setInterval(refresh,30000);
  window.addEventListener('focus', refresh);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });
})();
