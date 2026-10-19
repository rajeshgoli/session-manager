// Start now (sm#2383): one tap starts the job. The server declines, with one
// sentence, only when starting would endanger the Mac's memory; the owner
// confirms in place and the retry accepts that risk.
const inFlight = new Set();

// One request per job at a time, so a row and a panel tapped together still
// send a single start.
export async function requestStart(api, id, { acceptRisk = false } = {}) {
  if (inFlight.has(id)) return { started: false, busy: true };
  inFlight.add(id);
  try {
    const job = await api(`/client/queue/jobs/${encodeURIComponent(id)}/start`, {
      method: 'POST',
      body: { ask_if_risky: !acceptRisk },
    });
    return { started: true, job };
  } catch (error) {
    if (error?.body?.code === 'start_risk') return { started: false, risk: error.body.risk };
    throw error;
  } finally {
    inFlight.delete(id);
  }
}
