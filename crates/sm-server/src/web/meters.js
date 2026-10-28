// The sidebar's usage meters (spec 2543 B.6): one line per provider, a second
// line only when a limit runs out before reset or most of the week will go
// unused, and the Mac's readings only when they are high. Analytics keeps the
// full detail.
import { html, api, usePoll, navigate, gigabytes, meterBand } from './ui.js';

const WEEK_MS = 7 * 86400000;
const WINDOW = { five_hour: ['5-hour', '5h'], week: ['Week', 'wk'] };

/** Today: the time; within a week: weekday and time; else the date. "3:00 pm" reads "3 pm". */
export function when(iso, now = Date.now()) {
  const at = Date.parse(iso || '');
  if (!Number.isFinite(at)) return '';
  const date = new Date(at);
  const time = date.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }).toLowerCase().replace(':00', '');
  if (new Date(now).toDateString() === date.toDateString()) return time;
  if (at - now < 6 * 86400000) return `${date.toLocaleDateString([], { weekday: 'short' })} ${time}`;
  return `${date.toLocaleDateString([], { day: 'numeric', month: 'short' })} ${time}`;
}

/** Today: "today"; within a week: the weekday; else the date. */
function day(iso, now) {
  const at = Date.parse(iso || '');
  if (!Number.isFinite(at)) return '';
  const date = new Date(at);
  if (new Date(now).toDateString() === date.toDateString()) return 'today';
  if (at - now < 6 * 86400000) return date.toLocaleDateString([], { weekday: 'short' });
  return date.toLocaleDateString([], { day: 'numeric', month: 'short' });
}

/** A five-hour window shows only when it runs out before reset or is at 60% or more. */
const shown = (m) => m.window !== 'five_hour' || m.pace?.kind === 'runs_out' || m.percent >= 60;

/**
 * Each provider's compact row: `windows` ({ key, short, percent, text, band, title }) and
 * `notes` ({ text, tone }). Claude always shows a week window; a provider with no
 * current week window shows "wk –".
 */
export function usageRows(usage, now = Date.now()) {
  if (!usage) return [];
  return ['claude', 'codex'].map((provider) => {
    const mine = (usage.meters || []).filter((m) => m.provider === provider);
    // Several accounts with current windows each name theirs.
    const several = new Set(mine.map((m) => m.account_key)).size > 1;
    const order = (m) => (m.window === 'five_hour' ? 0 : m.scope ? 2 : 1);
    const meters = [...mine].sort((a, b) => order(a) - order(b));
    const named = (m) => {
      const [long, short] = WINDOW[m.window] || [m.window, m.window];
      const account = several ? (m.label || m.account_key).split('@')[0] : '';
      const base = m.scope ? { long: `${m.scope} ${m.window === 'five_hour' ? '5-hour' : 'week'}`, short: m.scope.slice(0, 2) } : { long, short };
      return account ? { long: `${base.long} · ${account}`, short: `${base.short} ${account}` } : base;
    };
    const windows = meters.filter(shown).map((m) => {
      const name = named(m);
      const pct = Math.round(m.percent);
      const pace = m.pace?.kind === 'runs_out' ? ` · runs out ${when(m.pace.at, now)}`
        : m.pace?.kind === 'on_pace' ? ` · on pace for ${Math.round(m.pace.percent)}% at reset` : '';
      return {
        key: `${m.account_key}:${m.window}:${m.scope || ''}`, short: name.short, percent: m.percent, text: `${pct}%`,
        band: meterBand(m.percent / 100),
        title: `${name.long}${m.label ? ` (${m.label})` : ''}: ${pct}% used · resets ${when(m.resets_at, now)}${pace} · read ${when(m.observed_at, now)}`,
      };
    });
    if (!meters.some((m) => m.window === 'week' && !m.scope)) {
      windows.splice(meters.filter((m) => m.window === 'five_hour' && shown(m)).length, 0,
        { key: 'week:none', short: 'wk', percent: 0, text: '–', band: 'green', title: 'Week: no reading' });
    }
    const out = meters.filter((m) => m.pace?.kind === 'runs_out');
    const notes = out.length
      ? out.map((m) => ({ text: `${named(m).long} runs out ${when(m.pace.at, now)}`, tone: Math.round(m.percent) >= 85 ? 'red' : 'amber' }))
      : meters.filter((m) => {
        const resets = Date.parse(m.resets_at || '');
        return m.window === 'week' && m.pace?.kind === 'on_pace' && m.pace.percent < 50
          && Number.isFinite(resets) && resets - now < WEEK_MS / 2;
      }).map((m, _, all) => {
        const p = Math.round(m.pace.percent);
        const lead = all.length > 1 || m.scope || several ? `${named(m).long}: on pace` : 'On pace';
        return { text: `${lead} for ${p}% · about ${100 - p}% unused at reset ${day(m.resets_at, now)}`, tone: 'cyan' };
      });
    return { provider, label: provider === 'claude' ? 'Claude' : 'Codex', windows, notes };
  });
}

/**
 * The Mac's readings that are amber or red (60% or more), plus `title` with all three.
 * Null when there is no host reading.
 */
export function macReadings(host) {
  if (!host || host.available === false) return null;
  const total = host.memory_total_bytes;
  const memPct = total ? (100 * (host.memory_used_bytes || 0)) / total : 0;
  const gpu = typeof host.gpu_percent === 'number' ? host.gpu_percent : null;
  const memText = `${gigabytes(host.memory_used_bytes)}/${gigabytes(total)}G`;
  const all = [
    { key: 'cpu', short: 'CPU', percent: host.cpu_percent || 0, text: `${Math.round(host.cpu_percent || 0)}%` },
    { key: 'memory', short: 'Mem', percent: memPct, text: memText },
    { key: 'gpu', short: 'GPU', percent: gpu || 0, text: gpu === null ? '–' : `${Math.round(gpu)}%` },
  ].map((r) => ({ ...r, band: meterBand(r.percent / 100, r.key) }));
  return {
    readings: all.filter((r) => r.band === 'amber' || r.band === 'red'),
    title: `CPU ${all[0].text} · Memory ${memText} · GPU ${all[2].text}`,
  };
}

function Window({ short, percent, text, band, title }) {
  return html`<span class="dw" title=${title}><i>${short}</i><u><s class=${band} style=${`width:${Math.max(0, Math.min(100, percent || 0))}%`}></s></u><b class=${band === 'green' || band === 'cyan' ? '' : band}>${text}</b></span>`;
}

export function Dash() {
  // The rail's meters are the shell's own reads, never the owner's (spec 2170 A.5).
  const [host] = usePoll(() => api('/client/host-status'), 10000, [], { background: true });
  const [usage] = usePoll(() => api('/client/usage/meters'), 30000, [], { background: true });
  const mac = macReadings(host);
  const open = (e) => { e.preventDefault(); navigate('/analytics/spend'); };
  return html`<div class="dash" aria-label="Usage">
    ${usageRows(usage).map(({ provider, label, windows, notes }) => html`
      <div class="dr" key=${provider}>
        <a class="dk" href="/analytics/spend" title=${`${label} usage · open Analytics`} onClick=${open}>${label}</a>
        ${windows.map((w) => html`<${Window} key=${w.key} ...${w} />`)}
      </div>
      ${notes.map((n) => html`<div class=${`dn ${n.tone}`}>${n.text}</div>`)}`)}
    ${mac ? (mac.readings.length
      ? mac.readings.map((r, i) => html`<div class="dr" key=${r.key} title=${mac.title}><span class="dk">${i ? '' : 'Mac'}</span><${Window} ...${r} title=${mac.title} /></div>`)
      : html`<div class="dr" title=${mac.title}><span class="dk">Mac</span><span class="quiet">quiet</span></div>`) : null}
  </div>`;
}
