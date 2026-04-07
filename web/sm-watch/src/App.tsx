import React, { useEffect, useMemo, useState } from 'react';
import {
  Activity,
  Bug,
  Cable,
  Layers3,
  Pause,
  Play,
  Search,
  Shield,
  Sparkles,
  Wifi,
  WifiOff,
  X,
} from 'lucide-react';
import { Session, SessionDetail, ToolCallRow, ActivityActionRow } from './types';
import {
  StatusFilter,
  ageFromIso,
  buildWatchRows,
  buildWatchSections,
  filterWatchSections,
  formatDateTime,
  sessionDisplayName,
  statsFromSessions,
} from './watchModel';
import { WatchTable } from './components/WatchTable';

const API_PATHS = ['/client/sessions', '/sessions', '/api/sessions'];
const POLL_MS = 4000;
const DETAIL_STALE_MS = 12000;
const KILL_PATH = '/sessions/{id}/kill';
const BUG_REPORT_PATH = '/client/bug-reports';

async function fetchJson<T>(paths: string[]): Promise<T | null> {
  for (const path of paths) {
    try {
      const response = await fetch(path, { cache: 'no-store' });
      if (response.status === 401) {
        const nextPath = window.location.pathname || '/watch/';
        window.location.assign(`/auth/google/login?next=${encodeURIComponent(nextPath)}`);
        return null;
      }
      if (!response.ok) {
        continue;
      }
      return (await response.json()) as T;
    } catch {
      continue;
    }
  }
  return null;
}

function summarizeActionRows(session: Session, payload: { tool_calls?: ToolCallRow[]; actions?: ActivityActionRow[] } | null): string[] {
  if (!payload) {
    return ['n/a (unavailable)'];
  }

  if (session.provider === 'codex-app') {
    const actions = payload.actions || [];
    if (actions.length === 0) {
      return ['-'];
    }
    return actions.slice(0, 10).map((action) => {
      const summary = action.summary_text || action.action_kind || 'action';
      const status = action.status ? ` [${action.status}]` : '';
      const when = action.ended_at || action.started_at;
      const age = when ? ` (${ageFromIso(when)})` : '';
      return `${summary}${status}${age}`;
    });
  }

  const toolCalls = payload.tool_calls || [];
  if (toolCalls.length === 0) {
    return session.provider === 'codex' ? ['n/a (no hooks)'] : ['-'];
  }
  return toolCalls.slice(0, 10).map((row) => {
    const tool = row.tool_name || '-';
    const age = row.timestamp ? ` (${ageFromIso(row.timestamp)})` : '';
    return `${tool}${age}`;
  });
}

function summarizeTail(output?: string | null): string[] {
  if (!output) {
    return ['-'];
  }
  const lines = output.split(/\r?\n/).filter((line, index, source) => line.length > 0 || index < source.length - 1);
  return lines.length > 0 ? lines.slice(-10) : ['-'];
}

async function fetchSessionDetail(session: Session): Promise<SessionDetail> {
  const sessionId = encodeURIComponent(session.id);
  const outputPromise = fetch(`/sessions/${sessionId}/output?lines=10`, { cache: 'no-store' });
  const activityPromise = session.provider === 'codex-app'
    ? fetch(`/sessions/${sessionId}/activity-actions?limit=10`, { cache: 'no-store' })
    : fetch(`/sessions/${sessionId}/tool-calls?limit=10`, { cache: 'no-store' });

  const [outputResponse, activityResponse] = await Promise.allSettled([outputPromise, activityPromise]);

  let lastError: string | null = null;
  let actionLines: string[] = ['n/a (unavailable)'];
  let tailLines: string[] = ['-'];

  if (activityResponse.status === 'fulfilled') {
    if (activityResponse.value.ok) {
      const activityPayload = (await activityResponse.value.json()) as { tool_calls?: ToolCallRow[]; actions?: ActivityActionRow[] };
      actionLines = summarizeActionRows(session, activityPayload);
    } else {
      lastError = `actions endpoint returned ${activityResponse.value.status}`;
    }
  } else {
    lastError = 'actions endpoint unavailable';
  }

  if (outputResponse.status === 'fulfilled') {
    if (outputResponse.value.ok) {
      const outputPayload = (await outputResponse.value.json()) as { output?: string | null };
      tailLines = summarizeTail(outputPayload.output);
    } else if (!lastError) {
      lastError = `output endpoint returned ${outputResponse.value.status}`;
    }
  } else if (!lastError) {
    lastError = 'output endpoint unavailable';
  }

  return {
    action_lines: actionLines,
    tail_lines: tailLines,
    fetched_at: Date.now(),
    loading: false,
    last_error: lastError,
  };
}

export default function App() {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [searchQuery, setSearchQuery] = useState('');
  const [filter, setFilter] = useState<StatusFilter>('all');
  const [isPaused, setIsPaused] = useState(false);
  const [isConnected, setIsConnected] = useState(false);
  const [lastSync, setLastSync] = useState<Date | null>(null);
  const [expandedSessions, setExpandedSessions] = useState<Set<string>>(new Set());
  const [sessionDetails, setSessionDetails] = useState<Record<string, SessionDetail>>({});
  const [isOpeningTelegram, setIsOpeningTelegram] = useState<string | null>(null);
  const [copiedAttachSessionId, setCopiedAttachSessionId] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isBugReportOpen, setIsBugReportOpen] = useState(false);
  const [bugReportText, setBugReportText] = useState('');
  const [bugReportIncludeDebugState, setBugReportIncludeDebugState] = useState(true);
  const [bugReportSessionId, setBugReportSessionId] = useState<string | null>(null);
  const [isSubmittingBugReport, setIsSubmittingBugReport] = useState(false);

  const pollSessions = async () => {
    if (isPaused) {
      return;
    }
    const payload = await fetchJson<{ sessions?: Session[] }>(API_PATHS);
    if (!payload || !Array.isArray(payload.sessions)) {
      setIsConnected(false);
      setError('Session list unavailable. Check API connectivity.');
      return;
    }
    setSessions(payload.sessions);
    setIsConnected(true);
    setLastSync(new Date());
    setError(null);
  };

  useEffect(() => {
    void pollSessions();
    const timer = window.setInterval(() => {
      void pollSessions();
    }, POLL_MS);
    return () => window.clearInterval(timer);
  }, [isPaused]);

  useEffect(() => {
    if (!toast) {
      return;
    }
    const timer = window.setTimeout(() => setToast(null), 2600);
    return () => window.clearTimeout(timer);
  }, [toast]);

  useEffect(() => {
    if (!copiedAttachSessionId) {
      return;
    }
    const timer = window.setTimeout(() => setCopiedAttachSessionId(null), 1800);
    return () => window.clearTimeout(timer);
  }, [copiedAttachSessionId]);

  useEffect(() => {
    const relevantSessions = sessions.filter((session) => expandedSessions.has(session.id));
    for (const session of relevantSessions) {
      const existing = sessionDetails[session.id];
      const isStale = !existing || (!existing.loading && Date.now() - existing.fetched_at > DETAIL_STALE_MS);
      if (!isStale || existing?.loading) {
        continue;
      }

      setSessionDetails((prev) => ({
        ...prev,
        [session.id]: prev[session.id]
          ? { ...prev[session.id], loading: true }
          : { action_lines: [], tail_lines: [], fetched_at: 0, loading: true },
      }));

      void fetchSessionDetail(session)
        .then((detail) => {
          setSessionDetails((prev) => ({ ...prev, [session.id]: detail }));
        })
        .catch(() => {
          setSessionDetails((prev) => ({
            ...prev,
            [session.id]: {
              action_lines: ['n/a (unavailable)'],
              tail_lines: ['-'],
              fetched_at: Date.now(),
              loading: false,
              last_error: 'detail fetch failed',
            },
          }));
        });
    }
  }, [expandedSessions, sessionDetails, sessions]);

  const sessionsById = useMemo(() => new Map(sessions.map((session) => [session.id, session])), [sessions]);
  const sections = useMemo(() => buildWatchSections(sessions), [sessions]);
  const filteredSections = useMemo(
    () => filterWatchSections(sections, filter, searchQuery),
    [sections, filter, searchQuery],
  );
  const rows = useMemo(
    () => buildWatchRows(filteredSections, sessionsById, expandedSessions, sessionDetails),
    [filteredSections, sessionsById, expandedSessions, sessionDetails],
  );
  const stats = useMemo(() => statsFromSessions(sessions), [sessions]);

  const showToast = (message: string) => setToast(message);

  const openBugReport = (sessionId?: string | null) => {
    setBugReportSessionId(sessionId || null);
    setBugReportText('');
    setBugReportIncludeDebugState(true);
    setIsBugReportOpen(true);
  };

  const closeBugReport = (force = false) => {
    if (isSubmittingBugReport && !force) {
      return;
    }
    setIsBugReportOpen(false);
    setBugReportText('');
    setBugReportSessionId(null);
    setBugReportIncludeDebugState(true);
  };

  const toggleExpand = (sessionId: string) => {
    setExpandedSessions((prev) => {
      const next = new Set(prev);
      if (next.has(sessionId)) {
        next.delete(sessionId);
      } else {
        next.add(sessionId);
      }
      return next;
    });
  };

  const handleOpenTelegram = (session: Session) => {
    if (!session.telegram_chat_id) {
      showToast('No Telegram thread is linked to this session yet.');
      return;
    }

    const normalizedChatId = String(Math.abs(session.telegram_chat_id)).replace(/^100/, '');
    const link = session.telegram_thread_id
      ? `https://t.me/c/${normalizedChatId}/${session.telegram_thread_id}`
      : `https://t.me/c/${normalizedChatId}`;

    setIsOpeningTelegram(session.id);
    const opened = window.open(link, '_blank');
    if (!opened) {
      showToast('Unable to open Telegram link. Check popup blocker settings.');
    }
    window.setTimeout(() => setIsOpeningTelegram(null), 180);
  };

  const handleCopyAttach = async (session: Session) => {
    const attach = session.termux_attach;
    if (!attach?.supported) {
      showToast(attach?.reason || 'Attach command is not available for this session.');
      return;
    }
    const command = attach.ssh_command || (attach.ssh_host && attach.ssh_username && attach.tmux_session
      ? `ssh -t ${attach.ssh_username}@${attach.ssh_host} 'tmux attach-session -t ${attach.tmux_session.replace(/'/g, `'"'"'`)}'`
      : null);
    if (!command) {
      showToast(attach?.reason || 'Attach command is not available for this session.');
      return;
    }
    try {
      await navigator.clipboard.writeText(command);
      setCopiedAttachSessionId(session.id);
      showToast(`Attach command copied for ${sessionDisplayName(session)}.`);
    } catch {
      showToast('Clipboard copy failed.');
    }
  };

  const postKill = async (id: string) => {
    const path = KILL_PATH.replace('{id}', encodeURIComponent(id));
    const response = await fetch(path, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({}),
    });

    let payload: { status?: string; error?: string } | null = null;
    try {
      payload = (await response.json()) as { status?: string; error?: string };
    } catch {
      payload = null;
    }

    return {
      ok: response.ok,
      httpStatus: response.status,
      killed: payload?.status === 'killed',
      error: payload?.error,
    };
  };

  const handleKillSession = async (id: string, event: React.MouseEvent) => {
    event.stopPropagation();
    try {
      const result = await postKill(id);
      if (!result.ok) {
        if (result.httpStatus === 422) {
          showToast('Kill request rejected by API payload validation.');
          return;
        }
        showToast('Kill request failed. Session manager endpoint not reachable.');
        return;
      }
      if (!result.killed) {
        showToast(result.error || 'Kill request failed.');
        return;
      }
      setSessions((prev) => prev.filter((session) => session.id !== id));
      setExpandedSessions((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
      setSessionDetails((prev) => {
        const next = { ...prev };
        delete next[id];
        return next;
      });
      showToast(`Killed ${id}.`);
    } catch {
      showToast('Kill request failed.');
    }
  };

  const handleSubmitBugReport = async () => {
    const reportText = bugReportText.trim();
    if (!reportText) {
      showToast('Describe what went wrong first.');
      return;
    }

    const clientState = {
      route: `${window.location.pathname}${window.location.search}`,
      search_query: searchQuery,
      filter,
      selected_session_id: bugReportSessionId,
      expanded_session_ids: Array.from(expandedSessions),
      visible_session_ids: sessions.map((session) => session.id),
      last_sync_at: lastSync ? lastSync.toISOString() : null,
      visible_error: error,
      visible_toast: toast,
      user_agent: navigator.userAgent,
    };

    setIsSubmittingBugReport(true);
    try {
      const response = await fetch(BUG_REPORT_PATH, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          report_text: reportText,
          include_debug_state: bugReportIncludeDebugState,
          selected_session_id: bugReportSessionId,
          client_state: clientState,
        }),
      });

      let payload: { bug_id?: string; detail?: string } | null = null;
      try {
        payload = (await response.json()) as { bug_id?: string; detail?: string };
      } catch {
        payload = null;
      }

      if (!response.ok) {
        showToast(payload?.detail || 'Bug report failed.');
        return;
      }

      closeBugReport(true);
      showToast(`Bug report submitted${payload?.bug_id ? `: ${payload.bug_id}` : '.'}`);
    } catch {
      showToast('Bug report failed.');
    } finally {
      setIsSubmittingBugReport(false);
    }
  };

  const bugReportSession = bugReportSessionId ? sessionsById.get(bugReportSessionId) || null : null;

  return (
    <div className="min-h-screen bg-[radial-gradient(circle_at_top,_rgba(34,211,238,0.12),_transparent_28%),linear-gradient(180deg,_#0b0b10_0%,_#121219_28%,_#09090d_100%)] text-zinc-100 selection:bg-cyan-400/30">
      <div className="mx-auto flex min-h-screen w-full max-w-[1440px] flex-col px-4 pb-10 pt-5 sm:px-6 lg:px-8">
        <header className="mb-5 rounded-[28px] border border-zinc-800 bg-zinc-950/80 px-5 py-4 shadow-[0_18px_60px_rgba(0,0,0,0.35)] backdrop-blur-xl">
          <div className="flex flex-col gap-4 lg:flex-row lg:items-start lg:justify-between">
            <div>
              <div className="mb-2 inline-flex items-center gap-2 rounded-full border border-cyan-500/20 bg-cyan-500/8 px-3 py-1 text-[11px] font-semibold uppercase tracking-[0.28em] text-cyan-200">
                <Sparkles size={12} />
                Android watch surface
              </div>
              <h1 className="text-3xl font-semibold tracking-[-0.04em] text-zinc-50">sm watch</h1>
              <p className="mt-1 max-w-3xl text-sm leading-6 text-zinc-400">
                Dense, dark, watch-first session control. Same hierarchy and detail model as the terminal watch view, but laid out for mobile and Android-side expansion.
              </p>
            </div>

            <div className="flex flex-wrap items-center gap-2 lg:justify-end">
              <div className="inline-flex items-center gap-2 rounded-full border border-zinc-800 bg-zinc-900/80 px-3 py-2 text-[11px] font-semibold uppercase tracking-[0.18em] text-zinc-300">
                {isConnected ? <Wifi size={13} className="text-emerald-300" /> : <WifiOff size={13} className="text-rose-300" />}
                {isConnected ? 'live' : 'offline'}
              </div>
              <div className="rounded-full border border-zinc-800 bg-zinc-900/80 px-3 py-2 font-mono text-[11px] text-zinc-400">
                Sync {lastSync ? formatDateTime(lastSync.toISOString()) : 'pending'}
              </div>
              <button
                type="button"
                onClick={() => setIsPaused((value) => !value)}
                className={`inline-flex items-center gap-2 rounded-full border px-3 py-2 text-[11px] font-semibold uppercase tracking-[0.18em] transition ${
                  isPaused
                    ? 'border-amber-500/40 bg-amber-500/10 text-amber-200'
                    : 'border-zinc-800 bg-zinc-900/80 text-zinc-200 hover:border-cyan-400/40 hover:text-cyan-100'
                }`}
              >
                {isPaused ? <Play size={13} /> : <Pause size={13} />}
                {isPaused ? 'resume polling' : 'pause polling'}
              </button>
              <button
                type="button"
                onClick={() => openBugReport(null)}
                className="inline-flex items-center gap-2 rounded-full border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-[11px] font-semibold uppercase tracking-[0.18em] text-amber-200 transition hover:border-amber-400/50 hover:text-amber-100"
              >
                <Bug size={13} />
                Report bug
              </button>
            </div>
          </div>
        </header>

        <section className="mb-5 grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
          {[
            { label: 'Sessions', value: stats.total, accent: 'text-zinc-50', icon: Layers3 },
            { label: 'Running / Working', value: `${stats.running} / ${stats.working}`, accent: 'text-emerald-300', icon: Activity },
            { label: 'Thinking', value: stats.thinking, accent: 'text-sky-300', icon: Cable },
            { label: 'EM / Maintainer', value: `${stats.em} / ${stats.maintainers}`, accent: 'text-cyan-300', icon: Shield },
          ].map((card) => (
            <button
              key={card.label}
              type="button"
              onClick={() => {
                if (card.label === 'Running / Working') {
                  setFilter((value) => (value === 'running' ? 'all' : 'running'));
                }
              }}
              className="rounded-[24px] border border-zinc-800 bg-zinc-950/80 px-4 py-4 text-left shadow-[0_14px_44px_rgba(0,0,0,0.3)] transition hover:border-zinc-700"
            >
              <div className="mb-3 inline-flex h-10 w-10 items-center justify-center rounded-2xl border border-zinc-800 bg-zinc-900/90 text-zinc-200">
                <card.icon size={18} />
              </div>
              <div className="text-[11px] font-semibold uppercase tracking-[0.24em] text-zinc-500">{card.label}</div>
              <div className={`mt-2 text-2xl font-semibold tracking-[-0.03em] ${card.accent}`}>{card.value}</div>
            </button>
          ))}
        </section>

        <section className="mb-5 rounded-[28px] border border-zinc-800 bg-zinc-950/80 p-4 shadow-[0_18px_60px_rgba(0,0,0,0.3)] backdrop-blur-xl">
          <div className="flex flex-col gap-4 xl:flex-row xl:items-center xl:justify-between">
            <label className="relative block flex-1">
              <Search size={16} className="pointer-events-none absolute left-4 top-1/2 -translate-y-1/2 text-zinc-500" />
              <input
                type="text"
                placeholder="Search name, id, role, alias, tmux session, working directory..."
                value={searchQuery}
                onChange={(event) => setSearchQuery(event.target.value)}
                className="w-full rounded-2xl border border-zinc-800 bg-zinc-950 px-11 py-3 text-sm text-zinc-100 outline-none transition placeholder:text-zinc-600 focus:border-cyan-400/50"
              />
            </label>
            <div className="flex flex-wrap gap-2">
              {(['all', 'running', 'idle', 'stopped'] as StatusFilter[]).map((candidate) => (
                <button
                  key={candidate}
                  type="button"
                  onClick={() => setFilter(candidate)}
                  className={`rounded-full border px-3 py-2 text-[11px] font-semibold uppercase tracking-[0.18em] transition ${
                    filter === candidate
                      ? 'border-cyan-400/40 bg-cyan-400/12 text-cyan-100'
                      : 'border-zinc-800 bg-zinc-900/80 text-zinc-400 hover:border-zinc-700 hover:text-zinc-200'
                  }`}
                >
                  {candidate}
                </button>
              ))}
            </div>
          </div>
        </section>

        {error ? (
          <div className="mb-5 rounded-2xl border border-rose-500/30 bg-rose-500/10 px-4 py-3 text-sm text-rose-200">
            {error}
          </div>
        ) : null}

        {rows.length > 0 ? (
          <WatchTable
            rows={rows}
            expandedSessions={expandedSessions}
            isOpeningTelegram={isOpeningTelegram}
            copiedAttachSessionId={copiedAttachSessionId}
            onToggleExpand={toggleExpand}
            onOpenTelegram={handleOpenTelegram}
            onKillSession={handleKillSession}
            onCopyAttach={handleCopyAttach}
            onReportBug={(session) => openBugReport(session.id)}
          />
        ) : (
          <div className="flex flex-1 items-center justify-center rounded-[28px] border border-dashed border-zinc-800 bg-zinc-950/70 px-6 py-24 text-center shadow-[0_18px_60px_rgba(0,0,0,0.3)]">
            <div>
              <div className="mx-auto mb-4 flex h-16 w-16 items-center justify-center rounded-full border border-zinc-800 bg-zinc-900 text-zinc-500">
                <Activity size={24} />
              </div>
              <h2 className="text-xl font-semibold tracking-[-0.02em] text-zinc-100">No sessions matched</h2>
              <p className="mt-2 text-sm text-zinc-500">
                {searchQuery || filter !== 'all' ? 'Adjust your filters or search terms.' : 'Waiting for Session Manager to report live sessions.'}
              </p>
            </div>
          </div>
        )}
      </div>

      {isBugReportOpen ? (
        <div className="fixed inset-0 z-40 flex items-center justify-center bg-black/70 px-4 py-6 backdrop-blur-sm">
          <div className="w-full max-w-2xl rounded-[28px] border border-zinc-800 bg-zinc-950/95 p-5 shadow-[0_24px_80px_rgba(0,0,0,0.55)]">
            <div className="mb-4 flex items-start justify-between gap-4">
              <div>
                <div className="mb-2 inline-flex items-center gap-2 rounded-full border border-amber-500/20 bg-amber-500/8 px-3 py-1 text-[11px] font-semibold uppercase tracking-[0.24em] text-amber-200">
                  <Bug size={12} />
                  Report bug
                </div>
                <h2 className="text-2xl font-semibold tracking-[-0.03em] text-zinc-50">What went wrong?</h2>
                <p className="mt-1 text-sm text-zinc-400">
                  {bugReportSession
                    ? `This report will include a snapshot for ${sessionDisplayName(bugReportSession)} [${bugReportSession.id}].`
                    : 'This report will include the current watch state if debug capture stays enabled.'}
                </p>
              </div>
              <button
                type="button"
                onClick={() => closeBugReport()}
                disabled={isSubmittingBugReport}
                className="inline-flex h-10 w-10 items-center justify-center rounded-full border border-zinc-800 bg-zinc-900/80 text-zinc-400 transition hover:border-zinc-700 hover:text-zinc-100 disabled:cursor-not-allowed disabled:text-zinc-600"
              >
                <X size={16} />
              </button>
            </div>

            <textarea
              value={bugReportText}
              onChange={(event) => setBugReportText(event.target.value)}
              placeholder="Describe the bug in one or two sentences."
              className="min-h-[12rem] w-full rounded-[24px] border border-zinc-800 bg-zinc-950 px-4 py-4 text-sm leading-6 text-zinc-100 outline-none transition placeholder:text-zinc-600 focus:border-amber-400/50"
            />

            <label className="mt-4 flex items-center gap-3 rounded-2xl border border-zinc-800 bg-zinc-900/60 px-4 py-3 text-sm text-zinc-300">
              <input
                type="checkbox"
                checked={bugReportIncludeDebugState}
                onChange={(event) => setBugReportIncludeDebugState(event.target.checked)}
                className="h-4 w-4 rounded border-zinc-700 bg-zinc-950 text-amber-300"
              />
              Include app debug state
            </label>

            <div className="mt-5 flex flex-wrap items-center justify-end gap-2">
              <button
                type="button"
                onClick={() => closeBugReport()}
                disabled={isSubmittingBugReport}
                className="rounded-full border border-zinc-800 bg-zinc-900/80 px-4 py-2 text-[11px] font-semibold uppercase tracking-[0.18em] text-zinc-300 transition hover:border-zinc-700 hover:text-zinc-100 disabled:cursor-not-allowed disabled:text-zinc-500"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => void handleSubmitBugReport()}
                disabled={isSubmittingBugReport}
                className="inline-flex items-center gap-2 rounded-full border border-amber-500/30 bg-amber-500/10 px-4 py-2 text-[11px] font-semibold uppercase tracking-[0.18em] text-amber-200 transition hover:border-amber-400/50 hover:text-amber-100 disabled:cursor-not-allowed disabled:text-amber-500"
              >
                <Bug size={13} />
                {isSubmittingBugReport ? 'Submitting...' : 'Submit bug'}
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {toast ? (
        <div className="fixed bottom-5 left-1/2 z-40 w-[min(92vw,34rem)] -translate-x-1/2 rounded-full border border-zinc-700 bg-zinc-950/95 px-4 py-3 text-center text-sm text-zinc-100 shadow-[0_20px_60px_rgba(0,0,0,0.5)] backdrop-blur-xl">
          {toast}
        </div>
      ) : null}
    </div>
  );
}
