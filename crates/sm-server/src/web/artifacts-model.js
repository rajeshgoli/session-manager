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
export const artifactInventory = (rows, query = '') => rows.filter(a => a.state !== 'deleted' && (!query.trim() || [a.name,a.note,a.epic,a.repo].some(s => s?.toLowerCase().includes(query.toLowerCase()))))
  .sort((a,b) => (artifactBytes(b) ?? -1) - (artifactBytes(a) ?? -1) || a.name.localeCompare(b.name));
export const largestArtifacts = rows => artifactInventory(rows).filter(a => !needsDecision(a) && artifactBytes(a) >= 1e9).slice(0,5);
export const artifactSummary = a => a.investigating ? 'Agent investigating' : needsDecision(a) ? 'Cleanup proposed · review before deleting' : a.state === 'unrecorded' ? "Needs an agent’s record" : a.state === 'frozen' ? 'Compressed · restored when needed' : a.hot ? 'In recent use' : 'Saved work';
export const backupSummary = b => !b ? 'Backup status unavailable' : !b.enabled ? 'Backup is off' : b.confirmed_at ? `Backup confirmed ${b.confirmed_at.slice(0,10)}` : 'No confirmed backup yet';
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
