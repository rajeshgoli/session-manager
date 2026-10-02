// A Start now check is an owner-readable snapshot, never a capacity reservation.
export function startNowController(api, id, now = () => performance.now()) {
  let check = null, checkedAt = 0, busy = false;
  const path = `/client/queue/jobs/${encodeURIComponent(id)}`;
  return {
    get busy() { return busy; },
    cancel() { check = null; },
    async inspect() {
      if (busy) return null;
      busy = true; check = null;
      try { const value = await api(`${path}/start-check`); if (value.state !== 'pending') throw new Error('This job is no longer waiting. Refresh its details.'); check = {...value, checked_at: new Date().toISOString()}; checkedAt = now(); return check; }
      finally { busy = false; }
    },
    async confirm(state) {
      if (busy) return { submitted:false };
      if (state !== 'pending') { check = null; throw new Error('This job is no longer waiting.'); }
      if (!check || now() - checkedAt >= 30000) return { submitted:false, check:await this.inspect() };
      busy = true;
      try { await api(`${path}/start`, {method:'POST',body:{}});check=null;return {submitted:true}; }
      finally { busy=false; }
    },
  };
}
