// Settings › Auto PM and the What changed card (spec 2170 F.3, sm#2176):
// on/off, skip this week, resume, this week's budget, decisions waiting on
// the owner, live changes with 👍 / 👎, and the chip a changed screen shows.
import { useState } from 'preact/hooks';
import { html, api, usePoll, navigate, toast, useSwitch, stored, store, Toggle, readHref } from './ui.js';

export const STATE_TEXT = { live: 'Live', measuring: 'Measuring', rolling_back: 'Rolling back', kept: 'Kept', reverted: 'Reverted' };

const day = (ms) => (ms ? new Date(ms).toLocaleDateString([], { month: 'short', day: 'numeric' }) : '');
const points = (value) => (typeof value === 'number' ? String(Math.round(value * 10) / 10) : '—');

/** "2 live · 1 measuring": live changes counted by state, in a fixed order. */
export function liveSummary(changes) {
  const counts = {};
  for (const change of changes || []) counts[change.state] = (counts[change.state] || 0) + 1;
  const parts = ['live', 'measuring', 'rolling_back', 'kept']
    .filter((state) => counts[state])
    .map((state) => `${counts[state]} ${STATE_TEXT[state].toLowerCase()}`);
  return parts.length ? parts.join(' · ') : 'None live';
}

/** The dates line under a change: when it went live and when sm measures it. */
export function changeMeta(change, now = Date.now()) {
  if (change.state === 'reverted') return `Reverted ${day(change.ended_at_ms)}${change.decision_note ? ` · ${change.decision_note}` : ''}`;
  if (change.state === 'rolling_back') return `Rolling back${change.decision_note ? ` · ${change.decision_note}` : ''}`;
  if (change.state === 'kept') return `Kept ${day(change.kept_at_ms)} · live since ${day(change.live_at_ms)}`;
  const measured = change.measure_at_ms && change.measure_at_ms <= now ? 'measuring now' : `measured ${day(change.measure_at_ms)}`;
  return `Live since ${day(change.live_at_ms)} · ${measured}`;
}

/** The settings write for Skip this week: now while not skipped, null to undo. */
export function skipPatch(skipped, now = new Date()) {
  return { autopm: { skip_window_starting: skipped ? null : now.toISOString() } };
}

/** "7 of 20" points spent of the allowance. */
export function budgetText(provider) {
  if (!provider) return 'No meter data';
  if (provider.skipped) return 'Skipped this week';
  return `${points(provider.spent)} of ${points(provider.allowance)}`;
}

async function vote(change, up, reload) {
  try {
    await api('/client/autopm/feedback', { method: 'POST', body: { experiment_id: change.id, vote: up ? 'up' : 'down' } });
    if (!up) toast(`${change.title} is off. Only you can turn it back on.`);
  } catch (e) {
    toast(e.message);
  }
  reload();
}

function Thumbs({ change, reload }) {
  const can = change.state !== 'reverted';
  return html`<span class="autopm-thumbs">
    <button type="button" class=${`btn sm ${change.vote === 'up' ? 'on' : ''}`} aria-pressed=${change.vote === 'up'}
      aria-label="Thumbs up" disabled=${!can} onClick=${() => vote(change, true, reload)}>👍</button>
    <button type="button" class=${`btn sm ${change.vote === 'down' ? 'on' : ''}`} aria-pressed=${change.vote === 'down'}
      aria-label="Thumbs down: switch it off" disabled=${!can} onClick=${() => vote(change, false, reload)}>👎</button>
  </span>`;
}

function ChangeRow({ change, reload }) {
  return html`<div class="autopm-change">
    <div class="autopm-change-text"><strong>${change.title}</strong>
      ${change.line ? html`<p class="sub">${change.line}</p>` : null}
      <p class="sub">${changeMeta(change)}</p></div>
    <${Thumbs} change=${change} reload=${reload} />
  </div>`;
}

function EndedRow({ change, reload }) {
  const turnOn = async () => {
    try {
      await api(`/autopm/switches/${encodeURIComponent(change.switch_key)}`, { method: 'PUT', body: { on: true, reason: 'turned back on in Settings' } });
    } catch (e) {
      toast(e.message);
    }
    reload();
  };
  return html`<div class="autopm-change ended">
    <div class="autopm-change-text"><strong>${change.title}</strong><p class="sub">${changeMeta(change)}</p></div>
    ${change.switch_key ? html`<button type="button" class="btn sm" onClick=${turnOn}>Turn back on</button>` : null}
  </div>`;
}

/** The What changed card: live changes with optional thumbs, then recently ended ones. */
export function WhatChanged({ view, reload }) {
  return html`<section class="autopm-card" aria-label="What changed">
    <h3>What changed <span class="sub">${liveSummary(view.changes)}</span></h3>
    ${view.changes.length ? view.changes.map((change) => html`<${ChangeRow} key=${change.id} change=${change} reload=${reload} />`)
      : html`<p class="sub">Nothing the auto PM shipped is live.</p>`}
    <p class="sub">Thumbs are optional. 👎 switches that change off at once; only you can turn it back on.</p>
    ${view.ended.length ? html`<h4>Recently ended</h4>${view.ended.map((change) => html`<${EndedRow} key=${change.id} change=${change} reload=${reload} />`)}` : null}
  </section>`;
}

/** Settings › Auto PM. */
export function AutoPmSettings({ write }) {
  const [view, error, reload] = usePoll(() => api('/client/autopm'), 30000);
  const [busy, setBusy] = useState(false);
  const [asking, setAsking] = useState(false);
  if (!view) return html`<h2>Auto PM</h2>${error ? html`<p role="alert" class="settings-error">${error.message} <button class="btn sm" onClick=${reload}>Retry</button></p>` : html`<p class="sub">Loading…</p>`}`;
  const save = async (autopm) => {
    setBusy(true);
    try {
      await write('/client/settings', { autopm });
    } catch (e) {
      toast(e.message);
    } finally {
      setBusy(false);
      reload();
    }
  };
  const claude = view.budget?.claude, codex = view.budget?.codex;
  const resets = (claude || codex)?.resets_at;
  return html`<h2>Auto PM</h2>
    <p class="sub">sm's own product manager: it ships one measured, revertable change at a time.</p>
    <div class="settings-field"><label><span>Auto PM on</span>
      <${Toggle} label="Auto PM on" checked=${view.enabled} disabled=${busy}
        onChange=${(on) => (on ? save({ enabled: true }) : setAsking(true))} /></label>
      ${asking ? html`<span class="confirm">Turn off and hold every change?
        <button type="button" class="btn sm danger" onClick=${() => { setAsking(false); save({ enabled: false }); }}>Turn off</button>
        <button type="button" class="btn sm" onClick=${() => setAsking(false)}>Cancel</button></span>` : null}
      <p class="sub">Turning Auto PM off switches every change off and holds it.</p></div>
    ${view.paused_at ? html`<div class="settings-field"><label><span>Paused: four cycles, nothing kept</span>
      <button class="btn sm" disabled=${busy} onClick=${() => save({ paused_at: null })}>Resume</button></label></div>` : null}
    <div class="settings-field"><label><span>Skip this week</span>
      <${Toggle} label="Skip this week" checked=${view.skipped} disabled=${busy} onChange=${() => write('/client/settings', skipPatch(view.skipped)).catch((e) => toast(e.message)).finally(reload)} /></label>
      <p class="sub">${view.skipped ? `Allowance 0 until the ${day(Date.parse(resets))} reset` : 'Sets this week’s allowance to zero; agents mid-ticket stop safely.'}</p></div>
    <h3>This week's budget <span class="sub">(points)</span></h3>
    <div class="autopm-budget">
      <span>Claude</span><span>${budgetText(claude)}</span>
      <span>Codex</span><span>${budgetText(codex)}</span>
    </div>
    ${view.waiting.length ? html`<button type="button" class="autopm-waiting" onClick=${() => navigate('/inbox')}>
      <strong>Waiting on you: ${view.waiting.length} decision${view.waiting.length === 1 ? '' : 's'}</strong>
      <span class="sub">${view.waiting.map((x) => x.title).join(' · ')} · open the Inbox to answer</span></button>` : null}
    ${view.readout ? html`<p><a href=${readHref(view.readout.reader_path)}>Latest readout: ${view.readout.week || view.readout.title} ›</a></p>` : null}
    <${WhatChanged} view=${view} reload=${reload} />`;
}

// ---- the chip on a changed screen --------------------------------------------

const DISMISSED = 'sm-autopm-chips';
let chipView = null;

/**
 * The dismissible "✦ New" chip a build ticket puts on each screen its switch
 * changes. It shows only while the switch is on and the change is live.
 */
export function AutoPmChip({ switchKey }) {
  const on = useSwitch(switchKey);
  const [dismissed, setDismissed] = useState(() => stored(DISMISSED, []).includes(switchKey));
  const [view, , reload] = usePoll(() => (on && !dismissed ? api('/client/autopm').then((v) => (chipView = v)) : Promise.resolve(chipView)), 0, [on, dismissed]);
  const change = on && !dismissed ? view?.changes?.find((c) => c.switch_key === switchKey) : null;
  if (!change) return null;
  const dismiss = () => {
    store(DISMISSED, [...stored(DISMISSED, []), switchKey]);
    setDismissed(true);
  };
  return html`<div class="autopm-chip" role="note"><span>✦ New: ${change.title}</span>
    <${Thumbs} change=${change} reload=${reload} />
    <button type="button" class="btn sm" aria-label="Dismiss" onClick=${dismiss}>✕</button></div>`;
}

