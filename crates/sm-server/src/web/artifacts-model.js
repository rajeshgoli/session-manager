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
export const artifactSummary = a => a.investigating ? 'Agent investigating' : needsAgentFollowup(a) ? (a.state === 'deleted' ? 'Folder missing · agent follow-up needed' : 'Storage issue · agent follow-up needed') : needsDecision(a) ? 'Cleanup proposed · review before deleting' : a.state === 'unrecorded' ? "Needs an agent’s record" : a.state === 'frozen' ? 'Compressed · restored when needed' : a.hot ? 'In recent use' : 'Saved work';
export const backupSummary = b => !b ? 'Backup status unavailable' : !b.enabled ? 'Backup is off' : b.confirmed_at ? `Backup confirmed ${b.confirmed_at.slice(0,10)}` : 'No confirmed backup yet';
// The remote disk's latest self-update of its sm (#2264), or null.
export const backupUpdate = b => !b?.update ? null : b.update.ok
  ? `Remote disk: updated to ${b.update.to}${b.update.at ? ` on ${b.update.at.slice(0,10)}` : ''}`
  : `Remote disk: update to ${b.update.to} failed${b.update.at ? ` on ${b.update.at.slice(0,10)}` : ''}: ${b.update.error || 'unknown error'}`;
const HEALTH = {verified:'Verified', failing:'Failing', not_reported:'Not reported'};
export const backupRate = bps => bps == null || !Number.isFinite(bps) ? 'unknown' : `${Math.round(bps * 8 / 1e6)} Mbit/s`;
const when = at => new Date(at).toLocaleString([], {month:'short', day:'numeric', hour:'numeric', minute:'2-digit'});
// The Backup card (#2276), from the remote disk's acknowledgements; null when backup is off.
export function backupCard(b) {
  if (!b?.enabled) return null;
  const c = b.card || {};
  const d = c.disk;
  const last = c.last_night;
  const check = c.weekly_check;
  return {
    nights: (c.nights || []).map(n => ({date: n.date, ok: n.ok, mark: n.ok === true ? '✓' : n.ok === false ? '✗' : '·',
      detail: n.ok === true ? `${n.date}: confirmed` : `${n.date}: ${n.error || 'not confirmed'}`})),
    lastNight: last ? `Last night: ${storageSize(last.bytes)} in ${duration(last.seconds)}, confirmed ${when(last.confirmed_at)}` : 'No confirmed night yet',
    check: check ? `Weekly check ${check.date}: ${check.files_checked} files, ${check.ok ? 'all ok' : `${check.bad} bad`}` : 'Weekly check: not run yet',
    checkOk: !check || check.ok !== false,
    disk: d ? {
      used: Math.max(0, d.total_bytes - d.free_bytes), total: d.total_bytes,
      line: `${storageSize(Math.max(0, d.total_bytes - d.free_bytes))} used · ${storageSize(d.free_bytes)} free of ${storageSize(d.total_bytes)}`,
      holding: `Holding ${storageSize(d.holding_bytes)}${d.holding_oldest ? ` · oldest ${d.holding_oldest}` : ''}`,
      pruned: d.pruned_on ? `Holding shortened for space on ${d.pruned_on}` : null,
      health: HEALTH[d.health] || 'Not reported', failing: d.health === 'failing',
    } : null,
    rates: [['Download', b.fetch_rate], ['Disk', b.apply_rate], ['Restore', b.probe_rate]].map(([name, r]) => `${name} ${backupRate(r)}`).join(' · '),
    inForce: backupRate(b.effective_rate),
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
