export function artifactSections(rows, epic = '') {
  const list = rows.filter(a => (!epic || a.epic === epic) && (a.state !== 'deleted' || a.needs_owner));
  const groups = [['Needs you', a => a.needs_owner], ['Hot', a => !a.needs_owner && a.hot && a.state !== 'frozen'],
    ['Cold, live', a => !a.needs_owner && !a.hot && a.state !== 'frozen'], ['Frozen', a => !a.needs_owner && a.state === 'frozen']];
  return groups.map(([title, test]) => ({ title, rows: list.filter(test).sort((a,b) => artifactBytes(b)-artifactBytes(a)) })).filter(g => g.rows.length);
}
export const artifactBytes = a => (a.state === 'frozen' ? a.frozen_bytes : a.disk_bytes) || 0;
export const duration = seconds => seconds == null || !Number.isFinite(seconds) ? 'unknown' : seconds < 60 ? `${Math.ceil(seconds)}s` : `${Math.ceil(seconds / 60)} min`;
export function backupLabel(a) {
  const returning = a.backup_return_seconds == null ? 'return time not measured' : `estimated return ${duration(a.backup_return_seconds)}`;
  if (!a.backup_due && a.backed_up_at) return a.rebuildable ? `backed up · ${returning} versus rebuild ${duration(a.rebuild_seconds)} · confirmed ${a.backed_up_at}` : 'backed up (not rebuildable)';
  return a.rebuildable ? `not backed up: rebuilds in ${duration(a.rebuild_seconds)}; ${returning}` : 'not backed up (not rebuildable)';
}

export function backupNights(runs, now = new Date()) {
  return Array.from({length:7},(_,i)=>{
    const day = new Date(now); day.setDate(day.getDate() - (6-i));
    const date = day.toLocaleDateString('en-CA');
    const run = runs.find(r => new Date(r.started_at).toLocaleDateString('en-CA') === date);
    return {date, mark:run ? (['acked','succeeded'].includes(run.state) ? '✓' : '✗') : '—'};
  });
}
export function age(iso) {
  const days = Math.floor((Date.now()-Date.parse(iso))/86400000);
  return Number.isFinite(days) ? days < 1 ? 'today' : `${days}d ago` : 'age unknown';
}
