import { useState } from 'preact/hooks';
import { html, api, usePoll, navigate, openPanel, Seg, duration, clock } from './ui.js';
import { HeldBack } from './queue.js';
import { jobAgentLabel } from './queue-model.js';

const percent = (n) => `${(n || 0).toFixed(1)}%`;
const number = (n) => (n || 0).toLocaleString();
const colors = ['amber', 'cyan', 'green', 'magenta', 'slate', 'codex', 'claude'];
export function AnalyticsPage({ path }) {
  const section = path.split('/')[2] || 'spend';
  return html`<div class="content analytics-page"><div><${Seg} label="Analytics section" value=${section} options=${['spend', 'time', 'queue'].map((value) => ({ value, label: value[0].toUpperCase() + value.slice(1) }))} onChange=${(value) => navigate(`/analytics/${value}`)} /></div>
    ${section === 'queue' ? html`<${QueueAnalytics} />` : html`<${DrillReport} key=${section} section=${section === 'time' ? 'time' : 'spend'} />`}
  </div>`;
}
function QueueAnalytics() {
  const [hours, setHours] = useState(24);
  const [stats, error] = usePoll(() => api(`/client/queue/stats?hours=${hours}`), 60000, [hours]);
  const [queue, queueError] = usePoll(() => api('/client/queue?ended_hours=24'), 30000);
  return html`<div><${Seg} label="Held back range" value=${hours} onChange=${setHours} options=${[{ value: 24, label: '24h' }, { value: 168, label: '7d' }, { value: 720, label: '30d' }]} /></div>
    ${error ? html`<p class="err">${error.message}</p>` : null}<${HeldBack} stats=${stats} />
    <section class="q-card"><h2>Stopped in the last 24 hours</h2>${queueError ? html`<p class="err">${queueError.message}</p>` : null}
      ${(queue?.ended || []).map((job) => html`<button class="a-row" onClick=${() => openPanel(`job:${job.id}`)}><span>${job.label || job.id}<small>${job.ended_summary || job.state} · ${jobAgentLabel(job)}</small></span><span>${clock(job.finished_at)} ›</span></button>`)}
      ${queue && !queue.ended.length ? html`<p class="muted">No stopped jobs in the last 24 hours.</p>` : null}
    </section>`;
}
function DrillReport({ section }) {
  const spend = section === 'spend';
  const [range, setRange] = useState(spend ? 'week' : '7d');
  const [provider, setProvider] = useState('');
  const [trail, setTrail] = useState([]);
  const url = `/client/analytics/${section}?range=${range}${spend && provider ? `&provider=${provider}` : ''}`;
  const [report, error] = usePoll(() => api(url), 60000, [url]);
  let node = report?.root;
  const crumbs = node ? [node] : [];
  for (const id of trail) { const next = node?.children?.find((n) => n.id === id); if (!next) break; node = next; crumbs.push(node); }
  const ranges = spend ? [['week', 'This week'], ['last_week', 'Last week'], ['4w', '4 weeks']] : [['24h', '24h'], ['7d', '7d'], ['30d', '30d']];
  const value = (n) => spend ? percent(n.percent) : duration(n.active_seconds);
  const labels = Object.fromEntries([...(report?.parts_legend || []), ...(report?.tool_legend || [])].map((l) => [l.key, l.label]));
  const parts = (n) => Object.entries(n.parts || {}).map(([key, v]) => html`<span style=${`color:var(--${colors[Math.max(0, (report?.parts_legend || []).findIndex((part) => part.key === key)) % colors.length]})`}>${labels[key] || key}: ${spend ? percent(v) : duration(v)}</span>`);
  const legend = report?.parts_legend || [];
  const composition = (child) => {
    const largest = Math.max(1, ...(node.children || []).map((n) => spend ? n.percent : n.active_seconds));
    return html`<span class="a-composition">${legend.map((part, i) => html`<i title=${`${part.label}: ${spend ? percent(child.parts[part.key]) : duration(child.parts[part.key] || 0)}`} style=${`width:${100 * (child.parts[part.key] || 0) / largest}%;background:var(--${colors[i % colors.length]})`}></i>`)}</span>`;
  };
  const enter = (child) => setTrail([...crumbs.slice(1).map((n) => n.id), child.id]);
  return html`<div class="q-heading">
    <${Seg} label="Analytics range" value=${range} onChange=${(v) => { setRange(v); setTrail([]); }} options=${ranges.map(([value, label]) => ({ value, label }))} />
    ${spend ? html`<${Seg} label="Provider" value=${provider || report?.provider || 'claude'} onChange=${(v) => { setProvider(v); setTrail([]); }} options=${[{ value: 'claude', label: 'Claude' }, { value: 'codex', label: 'Codex' }, { value: 'local', label: 'Local' }]} />` : null}
  </div>
  ${error ? html`<p class="err" role="alert">${error.message}</p>` : null}
  ${!report ? html`<p class="muted">Loading analytics…</p>` : report.provider === 'local' ? html`<${LocalSpend} report=${report} />` : html`
    ${spend ? html`<div class="q-tiles">${report.meters.map((meter) => html`<section class="q-card"><span class="q-label">${meter.label || meter.account_key}</span><strong>${percent(meter.percent)}</strong><p class="sub">Resets ${clock(meter.resets_at)} · observed ${clock(meter.observed_at)}</p>${meter.pace ? html`<p>${meter.pace.kind === 'runs_out' ? `Runs out ${clock(meter.pace.at)}` : `On pace for ${percent(meter.pace.percent)} at reset`}</p>` : null}<p class="sub">${meter.gap >= 0 ? 'Not in the ledger' : 'Ledger above meter'}: ${percent(Math.abs(meter.gap))}</p></section>`)}</div><p class="sub">${number(report.total.tokens)} tokens · estimated ${percent(report.total.percent)} of weekly allowance</p>${report.notes.map((note) => html`<p class="sub">${note}</p>`)}` : html`<p>${duration(report.total.active_seconds)} active · ${duration(report.total.parked_seconds)} parked · ${report.total.agents} agents</p>`}
    <section class="q-card"><nav class="a-crumbs" aria-label="Analytics drill-down">${crumbs.map((crumb, i) => html`<button class="btn sm" onClick=${() => setTrail(crumbs.slice(1, i + 1).map((n) => n.id))}>${i ? '› ' : ''}${crumb.label}</button>`)}</nav>
      <div class="q-heading"><h2>${node.label}</h2><strong>${value(node)}</strong></div><div class="q-legend">${parts(node)}</div>
      ${(node.children || []).map((child) => html`<button class="a-row" onClick=${() => enter(child)}><span>${child.label}<small>${child.kind}${child.state ? ` · ${child.state}` : ''}${!spend ? ` · parked ${duration(child.parked_seconds)}` : ` · ${number(child.tokens)} tokens`}</small>${composition(child)}<span class="a-parts">${parts(child)}</span></span><strong>${value(child)} ›</strong></button>`)}
      ${!node.children?.length ? html`<p class="sub">${node.session_status || 'No further breakdown.'}${node.turns != null ? ` · ${node.turns} turns` : ''}</p>` : null}
      ${node.session_id ? html`<button class="btn" onClick=${() => openPanel(`agent:${node.session_id}`)}>Open agent</button>` : null}
      ${node.history_path ? html`<a class="btn" href=${node.history_path}>History</a>` : null}
      ${(node.models || []).map((model) => html`<div class="a-row"><span>${model.model} · ${model.effort || 'default effort'}<small>${number(model.turns)} turns · input ${number(model.tokens.input)} · output ${number(model.tokens.output)} · cache write ${number(model.tokens.cache_write)} · cache read ${number(model.tokens.cache_read)}</small></span><strong>${percent(model.percent)}</strong></div>`)}
      ${Object.entries(node.tools || {}).map(([key, seconds]) => html`<div class="a-row"><span>${labels[key] || key}</span><span>${duration(seconds)}</span></div>`)}
    </section>`}`;
}

function LocalSpend({ report }) {
  const local = report.local;
  return html`<section class="q-card"><h2>Local</h2>
    <p>${number(report.total.tokens)} tokens · ${(local?.busy_hours || 0).toFixed(2)} model-busy hours</p>
    <h3>Tokens by model</h3>
    ${(local?.models || []).map((model) => html`<div class="a-row"><span>${model.model}<small>${number(model.turns)} turns · input ${number(model.tokens.input)} · output ${number(model.tokens.output)} · cache write ${number(model.tokens.cache_write)} · cache read ${number(model.tokens.cache_read)}</small></span></div>`)}
    ${!local?.models?.length ? html`<p class="sub">No local usage in this range.</p>` : null}
    <h3>Model-busy hours per day (UTC)</h3>
    ${(local?.days || []).map((day) => html`<div class="a-row"><span>${day.date}</span><strong>${day.busy_hours.toFixed(2)} h</strong></div>`)}
    ${report.notes.map((note) => html`<p class="sub">${note}</p>`)}
  </section>`;
}
