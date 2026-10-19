export const artifactBytes = a => a[a.state === 'frozen' ? 'frozen_bytes' : 'disk_bytes'] ?? null;
export function storageSize(bytes) {
  if (bytes == null || !Number.isFinite(bytes)) return 'Not measured';
  if (bytes === 0) return '0 MB';
  if (bytes < 1e6) return '<1 MB';
  const [unit,divisor] = bytes >= 1e12 ? ['TB',1e12] : bytes >= 1e9 ? ['GB',1e9] : ['MB',1e6];
  const value = bytes / divisor;
  return `${value.toFixed(value < 10 ? 1 : 0)} ${unit}`;
}
export const diskName = f => f.path.startsWith('/Volumes/') ? f.path.slice(9).split('/')[0] : 'Mac storage';
export const needsDecision = a => a.needs_you?.reasons?.includes('would_delete') || false;
export const needsAgentFollowup = a => a.needs_you?.reasons?.some(r => ['operation_failed','consumer_unverifiable','vanished'].includes(r)) || false;
export const artifactInventory = (rows, query = '') => rows.filter(a => (a.state !== 'deleted' || needsAgentFollowup(a)) && (!query.trim() || [a.name,a.note,a.epic,a.repo].some(s => s?.toLowerCase().includes(query.toLowerCase()))))
  .sort((a,b) => (artifactBytes(b) ?? -1) - (artifactBytes(a) ?? -1) || a.name.localeCompare(b.name));
export const largestArtifacts = rows => artifactInventory(rows).filter(a => !needsDecision(a) && !needsAgentFollowup(a) && artifactBytes(a) >= 1e9).slice(0,5);
export const artifactSummary = a => a.investigating ? 'Agent investigating' : artifactOp(a) ? artifactOp(a) : needsAgentFollowup(a) ? (a.state === 'deleted' ? 'Folder missing · agent follow-up needed' : 'Storage issue · agent follow-up needed') : needsDecision(a) ? 'Cleanup proposed · review before deleting' : a.state === 'unrecorded' ? "Needs an agent’s record" : a.state === 'frozen' ? `Frozen${a.placement ? ` on ${diskName({ path: a.placement.disk_path })}` : ''}${a.saved_bytes ? ` · saved ${storageSize(a.saved_bytes)}` : ''} · thawed when needed` : a.hot ? 'In recent use' : 'Saved work';
export const backupSummary = b => !b ? 'Backup status unavailable' : !b.enabled ? 'Backup is off' : b.confirmed_at ? `Backup confirmed ${b.confirmed_at.slice(0,10)}` : 'No confirmed backup yet';
// The remote disk's latest self-update of its sm (#2264), or null.
export const backupUpdate = b => !b?.update ? null : b.update.ok
  ? `Remote disk: updated to ${b.update.to}${b.update.at ? ` on ${b.update.at.slice(0,10)}` : ''}`
  : `Remote disk: update to ${b.update.to} failed${b.update.at ? ` on ${b.update.at.slice(0,10)}` : ''}: ${b.update.error || 'unknown error'}`;
const HEALTH = {verified:'Verified', failing:'Failing', not_reported:'Not reported'};
export const backupRate = bps => bps == null || !Number.isFinite(bps) ? 'unknown' : `${Math.round(bps * 8 / 1e6)} Mbit/s`;
const when = at => new Date(at).toLocaleString([], {month:'short', day:'numeric', hour:'numeric', minute:'2-digit'});
// The Backup card (#2276), from the remote disk's acknowledgements; null when backup is off.
// Each line appears only when it has something real to report (#2291).
export function backupCard(b) {
  if (!b?.enabled) return null;
  const c = b.card || {};
  const d = c.disk;
  const last = c.last_night;
  const check = c.weekly_check;
  const measured = c.measured || {};
  const nights = (c.nights || []).filter(n => n.ok === true || n.ok === false).map(n => ({date: n.date, ok: n.ok, mark: n.ok ? '✓' : '✗',
    detail: n.ok ? `${n.date}: confirmed` : `${n.date}: ${n.error || 'not confirmed'}`}));
  const latest = nights.at(-1);
  const rates = [['Download', measured.fetch, b.fetch_rate], ['Disk', measured.apply, b.apply_rate], ['Restore', measured.probe, b.probe_rate]]
    .filter(([, on]) => on).map(([name, , r]) => `${name} ${backupRate(r)}`);
  return {
    empty: !nights.length && !last && !check && !d,
    // One line at the top of the page, only when backup needs the owner.
    attention: d?.health === 'failing' ? 'Backup drive is failing. Replace it, then reseed with sm backup seed.'
      : check?.ok === false ? `Backup check found ${check.bad} damaged files on the remote disk`
      : latest?.ok === false ? `Last night's backup failed: ${latest.detail.slice(latest.date.length + 2)}` : null,
    nights,
    lastNight: last ? `Last backup: ${storageSize(last.bytes)} in ${duration(last.seconds)}, confirmed ${when(last.confirmed_at)}` : null,
    check: check ? `Weekly check ${check.date}: ${check.files_checked} files, ${check.ok ? 'all ok' : `${check.bad} bad`}` : null,
    checkOk: !check || check.ok !== false,
    disk: d ? {
      used: Math.max(0, d.total_bytes - d.free_bytes), total: d.total_bytes,
      line: `${storageSize(Math.max(0, d.total_bytes - d.free_bytes))} used · ${storageSize(d.free_bytes)} free of ${storageSize(d.total_bytes)}`,
      holding: `Holding ${storageSize(d.holding_bytes)}${d.holding_oldest ? ` · oldest ${d.holding_oldest}` : ''}`,
      pruned: d.pruned_on ? `Holding shortened for space on ${d.pruned_on}` : null,
      health: HEALTH[d.health] || 'Not reported', failing: d.health === 'failing',
    } : null,
    rates: rates.length ? `${rates.join(' · ')} · in force ${backupRate(b.effective_rate)}` : null,
  };
}
export const duration = seconds => seconds == null || !Number.isFinite(seconds) ? 'unknown' : seconds < 60 ? `${Math.ceil(seconds)}s` : `${Math.ceil(seconds / 60)} min`;
export function backupLabel(a) {
  const returning = a.backup_return_seconds == null ? 'return time not measured' : `estimated return ${duration(a.backup_return_seconds)}`;
  if (!a.backup_due && a.backed_up_at) return a.rebuildable ? `backed up · ${returning} versus rebuild ${duration(a.rebuild_seconds)} · confirmed ${a.backed_up_at}` : 'backed up (not rebuildable)';
  return a.rebuildable ? `not backed up: rebuilds in ${duration(a.rebuild_seconds)}; ${returning}` : 'not backed up (not rebuildable)';
}

export function artifactReport(a) {
  const text = a.report?.detail_json;
  if (!text) return null;
  try { return JSON.parse(text).note || text; } catch { return text; }
}

// ---- freeze, thaw and move (sm#2325) ---------------------------------------
// The wording matches sm's own: an operation is queued, copying, then done.
const OP = {
  freezing: { icon: 'freeze', tone: 'freeze', now: 'Freezing', past: 'Froze' },
  thawing: { icon: 'thaw', tone: 'thaw', now: 'Thawing', past: 'Thawed' },
  moving: { icon: 'move', tone: 'move', now: 'Moving to cold disk', past: 'Moved to cold disk' },
};
export const opKind = kind => OP[kind] || { icon: 'move', tone: 'move', now: kind, past: kind };
export const OP_STEPS = ['Queued', 'Copying', 'Finishing', 'Done'];
/** The step an operation has reached: queued 0, copying 1, finishing 2, done 3. */
export const opStep = op => ({ queued: 0, copying: 1, prepared: 2, cleanup: 2, done: 3, failed: -1 })[op.phase] ?? 0;
/** "45 s", "12 min", "1 h 20 min". */
export function span(seconds) {
  if (seconds == null || !Number.isFinite(seconds)) return null;
  if (seconds < 60) return `${Math.max(1, Math.round(seconds))} s`;
  const minutes = Math.round(seconds / 60);
  return minutes < 60 ? `${minutes} min` : `${Math.floor(minutes / 60)} h${minutes % 60 ? ` ${minutes % 60} min` : ''}`;
}
/** The line under an operation: where it is in line and when it will finish. */
export function opStatus(op) {
  const left = span(op.remaining_seconds);
  if (op.state === 'done') return `Done${span(op.seconds) ? ` · took ${span(op.seconds)}` : ''}`;
  if (op.state === 'failed') return `Failed: ${op.error || 'no reason recorded'}`;
  if (op.state === 'running') {
    const behind = op.behind ? ` · ${op.behind} waiting behind it` : '';
    if (op.over_estimate) return `Running now${behind} · taking longer than recent ${opKind(op.kind).now.toLowerCase()}`;
    if (op.remaining_seconds != null && op.remaining_seconds < 1) return `Running now${behind} · should finish any moment`;
    return `Running now${behind}${left ? ` · about ${left} left` : ''}`;
  }
  if (op.error) return /capacity/.test(op.error) ? 'Waiting for space on the cold disk above its floor' : `Waiting: ${op.error}`;
  const starts = span(op.starts_in_seconds);
  const ahead = op.ahead ? `${op.ahead} ahead` : 'next in line';
  return `Waiting · ${ahead}${starts && op.ahead ? ` · starts in about ${starts}` : ''}${left ? ` · done in about ${left}` : ''}`;
}
/** How far an estimated copy has run, 0–1, or null without an estimate. */
export const opProgress = op => op.state !== 'running' || !op.expected_seconds || op.elapsed_seconds == null ? null
  : Math.min(0.97, op.elapsed_seconds / op.expected_seconds);
/** Who asked for it: a session name with its id, the owner, or sm. */
export const actorLabel = op => op.actor_name && op.actor_name !== op.actor ? op.actor_name : op.actor;
export const actorId = op => op.actor_name && op.actor_name !== op.actor && /^[0-9a-f]{8}$/.test(op.actor) ? op.actor : null;
export const clock = at => !at ? '' : new Date(at).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
export const day = at => !at ? '' : new Date(at).toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
export const opName = op => op.names?.length > 1 ? `${op.names[0]} + ${op.names.length - 1} more` : op.names?.[0] || `operation ${op.id}`;
/** "344 GB → 37.6 GB" for a freeze, the data size otherwise. */
export const opSizes = op => op.kind === 'freezing' && op.data_bytes != null && op.frozen_bytes != null
  ? `${storageSize(op.data_bytes)} → ${storageSize(op.frozen_bytes)}` : op.data_bytes != null ? storageSize(op.data_bytes) : '';
export const opSaved = op => op.kind === 'freezing' && op.state === 'done' && op.data_bytes != null && op.frozen_bytes != null
  ? Math.max(0, op.data_bytes - op.frozen_bytes) : null;
export const ratio = (data, frozen) => data && frozen ? `${(data / frozen).toFixed(data / frozen < 10 ? 1 : 0)}×` : null;
/** The savings line under the big number. */
export function savingsLine(s) {
  if (!s?.folders) return 'No frozen folders';
  const disks = [...new Set((s.roots || []).filter(r => r.folders).map(r => diskName({ path: r.cold_store })))];
  return `${s.folders} frozen ${s.folders === 1 ? 'folder' : 'folders'} · ${storageSize(s.data_bytes)} of data in ${storageSize(s.frozen_bytes)} of archives${disks.length ? ` on ${disks.join(', ')}` : ''}`;
}
/** Where a folder's bytes are, in the owner's words. */
export function placementLabel(a) {
  const p = a.placement;
  if (!p) return null;
  const disk = diskName({ path: p.disk_path });
  return a.state === 'frozen' ? `Archive on ${disk}` : `${disk}${p.disk === 'cold' ? ' (cold disk)' : ''}`;
}
/** How long a thaw should take, and the last one measured. */
export function thawTime(a) {
  const eta = span(a.thaw_eta_seconds);
  const last = a.last_thaw?.seconds != null ? `last ${span(a.last_thaw.seconds)} on ${day(a.last_thaw.at)}` : null;
  return [eta && `about ${eta}`, last].filter(Boolean).join(' · ') || null;
}
/** A folder with an operation running or waiting on it. */
export const artifactOp = a => a.op ? `${opKind(a.op).now} · ${a.operation?.phase === 'queued' ? 'queued' : opStep(a.operation || {}) === 2 ? 'finishing' : 'copying'}` : null;
