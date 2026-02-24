import React, { useEffect, useMemo, useState } from 'react';
import {
  Activity,
  AlertCircle,
  ArrowRight,
  ChevronDown,
  ChevronUp,
  MessageCircle,
  Pause,
  Play,
  Search,
  ShieldAlert,
  Terminal,
  Trash2,
  Wifi,
  WifiOff,
} from 'lucide-react';
import { motion, AnimatePresence } from 'motion/react';
import { Session } from './types';

type SessionTreeNode = Session & { children: SessionTreeNode[] };
type StatusFilter = 'all' | Session['status'];

const STATUS_COLORS: Record<Session['status'], string> = {
  running: 'bg-emerald-100 text-emerald-700 border-emerald-200',
  idle: 'bg-slate-100 text-slate-600 border-slate-200',
  stopped: 'bg-rose-100 text-rose-700 border-rose-200',
};

const ACTIVITY_STATE_BADGE: Record<string, string> = {
  working: 'text-emerald-600',
  thinking: 'text-indigo-500',
  idle: 'text-slate-500',
  waiting: 'text-amber-500',
  stopped: 'text-rose-500',
};

const API_PATHS = ['/sessions', '/api/sessions'];
const KILL_PATH = '/sessions/{id}/kill';
const POLL_MS = 4000;

interface SessionCardProps {
  session: SessionTreeNode;
  expandedSessions: Set<string>;
  isPaused: boolean;
  isOpeningTelegram: string | null;
  onToggleExpand: (id: string, event: React.MouseEvent) => void;
  onOpenTelegram: (session: Session) => void;
  onKillSession: (id: string, event: React.MouseEvent) => Promise<void>;
  depth?: number;
}

function sessionDisplayName(session: Session): string {
  return (session.friendly_name && session.friendly_name.trim()) || session.name;
}

function formatDateTime(value: string): string {
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) {
    return value;
  }
  return parsed.toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}

function normalizeThreadChatId(chatId?: number | null): string {
  if (!chatId) {
    return '';
  }
  return String(Math.abs(chatId)).replace(/^100/, '');
}

function telegramDeepLink(chatId?: number | null, threadId?: number | null): string | null {
  if (!chatId) {
    return null;
  }

  const normalizedChatId = normalizeThreadChatId(chatId);
  if (threadId) {
    return `https://t.me/c/${normalizedChatId}/${threadId}`;
  }
  return `https://t.me/c/${normalizedChatId}`;
}

const SessionCard: React.FC<SessionCardProps> = ({
  session,
  expandedSessions,
  isPaused,
  isOpeningTelegram,
  onToggleExpand,
  onOpenTelegram,
  onKillSession,
  depth = 0,
}) => {
  const isExpanded = expandedSessions.has(session.id);

  return (
    <div className="space-y-3">
      <motion.div
        layout
        initial={{ opacity: 0, y: 12 }}
        animate={{ opacity: 1, y: 0 }}
        exit={{ opacity: 0, scale: 0.98 }}
        onClick={() => onOpenTelegram(session)}
        style={{ marginLeft: `${depth * 14}px` }}
        className="relative bg-white border border-slate-200 rounded-2xl p-4 shadow-sm active:scale-[0.99] transition-all cursor-pointer group"
      >
        <div className="flex items-start justify-between gap-2 mb-2">
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <div
                className={`w-2 h-2 rounded-full ${
                  session.status === 'running'
                    ? 'bg-emerald-500 shadow-[0_0_8px_rgba(16,185,129,0.5)]'
                    : session.status === 'stopped'
                      ? 'bg-rose-500 shadow-[0_0_8px_rgba(244,63,94,0.5)]'
                      : 'bg-slate-300'
                }`}
              />
              <h3 className="font-bold text-slate-900 truncate max-w-[18ch]">
                {sessionDisplayName(session)}
              </h3>
              {session.children.length > 0 && (
                <span className="bg-slate-100 text-slate-500 text-[10px] px-1.5 py-0.5 rounded-full">
                  {session.children.length}
                </span>
              )}
            </div>
            <p className="text-[11px] text-slate-500 mt-1 truncate">
              {session.id} • {session.role ? `#${session.role}` : `tmux: ${session.tmux_session}`}
            </p>
          </div>

          <button
            onClick={(event) => onKillSession(session.id, event)}
            className="p-1.5 text-slate-400 hover:text-rose-500 transition-colors"
            title="Kill session"
          >
            <Trash2 size={16} />
          </button>
        </div>

        <div className="grid grid-cols-2 gap-y-2 text-xs text-slate-500 mb-3">
          <span className={`inline-flex items-center rounded-md px-2 py-0.5 border text-[10px] font-bold uppercase w-fit ${STATUS_COLORS[session.status]}`}>
            {session.status}
          </span>
          <span className={`justify-self-end text-[10px] font-bold uppercase tracking-wide ${ACTIVITY_STATE_BADGE[session.activity_state || 'idle'] || 'text-slate-500'}`}>
            {session.activity_state || 'idle'}
          </span>

          <span className="text-[11px]">
            Last active: {formatDateTime(session.last_activity)}
          </span>
          <span className="justify-self-end text-[11px]">
            Tokens: {session.tokens_used ?? 0}
          </span>
        </div>

        {(session.current_task || session.agent_status_text || session.last_action_summary) && (
          <div className="mb-3 text-sm text-slate-700 bg-slate-50 rounded-xl border border-slate-100 p-2">
            {session.current_task || session.agent_status_text || session.last_action_summary}
          </div>
        )}

        <div className="flex items-center justify-between border-t border-slate-100 pt-3">
          <button
            onClick={(event) => onToggleExpand(session.id, event)}
            className="inline-flex items-center gap-1 text-[10px] font-bold uppercase tracking-wide text-slate-500 hover:text-slate-900 transition-colors"
          >
            {isExpanded ? <ChevronUp size={12} /> : <ChevronDown size={12} />}
            {isExpanded ? 'Hide details' : 'Show details'}
          </button>

          <span className="inline-flex items-center gap-1 text-blue-600 text-xs font-bold">
            <MessageCircle size={14} />
            <span>{isPaused ? 'Resume to refresh' : 'Tap to open Telegram thread'}</span>
            <ArrowRight size={12} />
          </span>
        </div>

        <AnimatePresence>
          {isExpanded && (
            <motion.div
              initial={{ height: 0, opacity: 0 }}
              animate={{ height: 'auto', opacity: 1 }}
              exit={{ height: 0, opacity: 0 }}
              className="overflow-hidden"
            >
              <div className="mt-3 p-3 bg-slate-900 text-slate-200 rounded-xl text-[11px] font-mono space-y-1">
                <div className="text-slate-500">Working dir</div>
                <div className="break-all">{session.working_dir}</div>
                <div className="text-slate-500 mt-2">Working on</div>
                <div>{session.current_task || 'No current task'}</div>
                <div className="text-slate-500 mt-2">Last tool</div>
                <div>{session.last_tool_name || 'Unknown'}</div>
                <div className="text-slate-500 mt-2">Git remote</div>
                <div className="break-all">{session.git_remote_url || 'N/A'}</div>
              </div>
            </motion.div>
          )}
        </AnimatePresence>

        <AnimatePresence>
          {isOpeningTelegram === session.id && (
            <motion.div
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
              className="absolute inset-0 bg-blue-600/90 rounded-2xl flex items-center justify-center text-white z-10"
            >
              <span className="text-xs font-bold uppercase tracking-wide">
                Opening Telegram thread...
              </span>
            </motion.div>
          )}
        </AnimatePresence>
      </motion.div>

      {session.children.length > 0 && (
        <div className="space-y-3">
          {session.children.map((child) => (
            <SessionCard
              key={child.id}
              session={child as SessionTreeNode}
              expandedSessions={expandedSessions}
              isPaused={isPaused}
              isOpeningTelegram={isOpeningTelegram}
              onToggleExpand={onToggleExpand}
              onOpenTelegram={onOpenTelegram}
              onKillSession={onKillSession}
              depth={depth + 1}
            />
          ))}
        </div>
      )}
    </div>
  );
};

function buildSessionTree(sessions: Session[]): SessionTreeNode[] {
  const nodes = new Map<string, SessionTreeNode>();
  const roots: SessionTreeNode[] = [];

  sessions.forEach((session) => {
    nodes.set(session.id, { ...session, children: [] });
  });

  sessions.forEach((session) => {
    const node = nodes.get(session.id)!;
    const parent = session.parent_session_id ? nodes.get(session.parent_session_id) : null;
    if (parent) {
      parent.children.push(node);
    } else {
      roots.push(node);
    }
  });

  nodes.forEach((node) => {
    node.children.sort((a, b) => b.created_at.localeCompare(a.created_at));
  });

  return roots.sort((a, b) => b.created_at.localeCompare(a.created_at));
}

function filterTree(
  nodes: SessionTreeNode[],
  statusFilter: StatusFilter,
  query: string,
): SessionTreeNode[] {
  const normalizedQuery = query.trim().toLowerCase();

  const matchesSearch = (session: Session): boolean => {
    if (!normalizedQuery) {
      return true;
    }
    return (
      session.id.toLowerCase().includes(normalizedQuery) ||
      session.name.toLowerCase().includes(normalizedQuery) ||
      (session.friendly_name || '').toLowerCase().includes(normalizedQuery) ||
      session.tmux_session.toLowerCase().includes(normalizedQuery) ||
      session.working_dir.toLowerCase().includes(normalizedQuery)
    );
  };

  const passesFilter = (session: Session): boolean => {
    return statusFilter === 'all' || session.status === statusFilter;
  };

  const recurse = (node: SessionTreeNode): SessionTreeNode | null => {
    const visibleChildren = node.children
      .map(recurse)
      .filter((child): child is SessionTreeNode => child !== null);

    if (matchesSearch(node) && passesFilter(node)) {
      return { ...node, children: visibleChildren };
    }

    if (visibleChildren.length > 0) {
      return { ...node, children: visibleChildren };
    }

    return null;
  };

  return nodes.map(recurse).filter((node): node is SessionTreeNode => node !== null);
}

async function fetchJson<T>(paths: string[]): Promise<T | null> {
  for (const path of paths) {
    try {
      const response = await fetch(path, {
        cache: 'no-store',
      });
      if (!response.ok) {
        continue;
      }
      return (await response.json()) as T;
    } catch (error) {
      continue;
    }
  }
  return null;
}

export default function App() {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [searchQuery, setSearchQuery] = useState('');
  const [filter, setFilter] = useState<StatusFilter>('all');
  const [isPaused, setIsPaused] = useState(false);
  const [isConnected, setIsConnected] = useState(false);
  const [lastSync, setLastSync] = useState<Date | null>(null);
  const [isOpeningTelegram, setIsOpeningTelegram] = useState<string | null>(null);
  const [expandedSessions, setExpandedSessions] = useState<Set<string>>(new Set());
  const [toast, setToast] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const toggleExpand = (id: string, event: React.MouseEvent) => {
    event.stopPropagation();
    setExpandedSessions((prev) => {
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  };

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
    return () => clearInterval(timer);
  }, [isPaused]);

  useEffect(() => {
    if (!toast) {
      return;
    }
    const timer = window.setTimeout(() => setToast(null), 2600);
    return () => clearTimeout(timer);
  }, [toast]);

  const stats = useMemo(() => {
    return {
      running: sessions.filter((session) => session.status === 'running').length,
      idle: sessions.filter((session) => session.status === 'idle').length,
      stopped: sessions.filter((session) => session.status === 'stopped').length,
      total: sessions.length,
    };
  }, [sessions]);

  const sessionTree = useMemo(() => buildSessionTree(sessions), [sessions]);

  const filteredTree = useMemo(
    () => filterTree(sessionTree, filter, searchQuery),
    [sessionTree, filter, searchQuery],
  );

  const showToast = (message: string) => {
    setToast(message);
  };

  const handleOpenTelegram = (session: Session) => {
    const link = telegramDeepLink(session.telegram_chat_id, session.telegram_thread_id);
    if (!link) {
      showToast('No Telegram thread is linked to this session yet.');
      return;
    }

    setIsOpeningTelegram(session.id);
    if (window.navigator.vibrate) {
      window.navigator.vibrate(35);
    }

    window.setTimeout(() => {
      window.open(link, '_blank');
      setIsOpeningTelegram(null);
    }, 150);
  };

  const postKill = async (id: string): Promise<{ ok: boolean; status?: number }> => {
    const path = KILL_PATH.replace('{id}', encodeURIComponent(id));
    try {
      const response = await fetch(path, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({}),
      });
      return { ok: response.ok, status: response.status };
    } catch (error) {
      return { ok: false };
    }
  };

  const handleKillSession = async (id: string, event: React.MouseEvent) => {
    event.stopPropagation();
    try {
      const result = await postKill(id);
      if (!result.ok) {
        if (result.status === 422) {
          showToast('Kill request rejected by API payload validation.');
          return;
        }
        showToast('Kill request failed. Session manager endpoint not reachable.');
        return;
      }
      setSessions((prev) => prev.filter((session) => session.id !== id));
      setExpandedSessions((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    } catch (err) {
      showToast('Kill request failed.');
      console.error('Failed to kill session', err);
    }
  };

  return (
    <div className="min-h-screen bg-[#f6f8fb] text-slate-900 font-sans selection:bg-blue-100">
      <header className="sticky top-0 z-30 bg-white/85 backdrop-blur-md border-b border-slate-200 px-4 py-3">
        <div className="max-w-2xl mx-auto flex items-center justify-between">
          <div className="flex items-center gap-2">
            <div className="bg-slate-900 p-1.5 rounded-lg shadow-sm">
              <Terminal size={20} className="text-white" />
            </div>
            <div>
              <h1 className="font-bold text-lg tracking-tight leading-none">sm watch</h1>
              <div className="flex items-center gap-1 mt-1">
                {isConnected ? (
                  <Wifi size={10} className="text-emerald-500" />
                ) : (
                  <WifiOff size={10} className="text-rose-500" />
                )}
                <span className="text-[10px] uppercase font-bold tracking-wide text-slate-500">
                  {isConnected ? 'Live' : 'Offline'} •{' '}
                  {lastSync?.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })}
                </span>
              </div>
            </div>
          </div>

          <button
            onClick={() => setIsPaused((value) => !value)}
            className={`p-2 rounded-full transition-colors ${
              isPaused ? 'bg-amber-100 text-amber-600' : 'hover:bg-slate-100 text-slate-500'
            }`}
            title={isPaused ? 'Resume polling' : 'Pause polling'}
          >
            {isPaused ? <Play size={20} fill="currentColor" /> : <Pause size={20} fill="currentColor" />}
          </button>
        </div>
      </header>

      <main className="max-w-2xl mx-auto pb-24">
        <div className="px-4 py-4 overflow-x-auto no-scrollbar flex gap-3">
          {[
            { label: 'Total', count: stats.total, filter: 'all' },
            { label: 'Running', count: stats.running, filter: 'running' },
            { label: 'Idle', count: stats.idle, filter: 'idle' },
            { label: 'Stopped', count: stats.stopped, filter: 'stopped' },
          ].map((item) => (
            <button
              key={item.label}
              onClick={() => setFilter(filter === item.filter ? 'all' : item.filter)}
              className={`flex-shrink-0 px-4 py-3 rounded-2xl border transition-all ${
                filter === item.filter
                  ? 'bg-white border-slate-900 shadow-sm ring-1 ring-slate-900'
                  : 'bg-white border-slate-200 shadow-sm'
              }`}
            >
              <div className="text-[10px] uppercase font-bold tracking-widest text-slate-400 mb-1">{item.label}</div>
              <div className="text-xl font-bold leading-none">{item.count}</div>
            </button>
          ))}
        </div>

        <div className="px-4">
          <div className="relative group">
            <Search
              className="absolute left-3 top-1/2 -translate-y-1/2 text-slate-400 transition-colors group-focus-within:text-slate-900"
              size={18}
            />
            <input
              type="text"
              placeholder="Search session name, id, working directory..."
              value={searchQuery}
              onChange={(event) => setSearchQuery(event.target.value)}
              className="w-full bg-white border border-slate-200 rounded-2xl py-3 pl-10 pr-4 focus:outline-none focus:ring-2 focus:ring-slate-900/5 focus:border-slate-900 transition-all shadow-sm"
            />
          </div>
        </div>

        <div className="px-4 mt-3 flex gap-2 overflow-x-auto no-scrollbar">
          {(['all', 'running', 'idle', 'stopped'] as StatusFilter[]).map((candidate) => (
            <button
              key={candidate}
              onClick={() => setFilter(candidate)}
              className={`px-4 py-1.5 rounded-full text-xs font-bold capitalize transition-all border ${
                filter === candidate
                  ? 'bg-slate-900 text-white border-slate-900'
                  : 'bg-white text-slate-500 border-slate-200'
              }`}
            >
              {candidate}
            </button>
          ))}
        </div>

        {error && (
          <div className="px-4 mt-4">
            <div className="bg-rose-50 border border-rose-200 text-rose-700 rounded-xl px-3 py-2 text-sm flex items-center gap-2">
              <AlertCircle size={14} />
              {error}
            </div>
          </div>
        )}

        <div className="px-4 mt-4 space-y-3">
          <AnimatePresence mode="popLayout">
            {filteredTree.length > 0 ? (
              filteredTree.map((session) => (
                <SessionCard
                  key={session.id}
                  session={session}
                  expandedSessions={expandedSessions}
                  isPaused={isPaused}
                  isOpeningTelegram={isOpeningTelegram}
                  onToggleExpand={toggleExpand}
                  onOpenTelegram={handleOpenTelegram}
                  onKillSession={handleKillSession}
                />
              ))
            ) : (
              <div className="py-20 text-center">
                <div className="bg-slate-100 w-16 h-16 rounded-full flex items-center justify-center mx-auto mb-4">
                  <Activity size={24} className="text-slate-300" />
                </div>
                <h3 className="font-bold text-slate-900">No sessions matched</h3>
                <p className="text-sm text-slate-500 mt-1">
                  {searchQuery || filter !== 'all' ? 'Try adjusting your search or filter.' : 'Waiting for sm sessions to appear.'}
                </p>
              </div>
            )}
          </AnimatePresence>
        </div>
      </main>

      <nav className="fixed bottom-0 left-0 right-0 bg-white/90 backdrop-blur-md border-t border-slate-200 px-6 py-3 flex justify-around items-center z-30">
        <button className="flex flex-col items-center gap-1 text-slate-900">
          <Terminal size={20} />
          <span className="text-[10px] font-bold uppercase tracking-tighter">Watch</span>
        </button>
        <button className="flex flex-col items-center gap-1 text-slate-400">
          <ShieldAlert size={20} />
          <span className="text-[10px] font-bold uppercase tracking-tighter">Alerts</span>
        </button>
        <button className="flex flex-col items-center gap-1 text-slate-400">
          <span>⚙️</span>
          <span className="text-[10px] font-bold uppercase tracking-tighter">Settings</span>
        </button>
      </nav>

      <AnimatePresence>
        {toast && (
          <motion.div
            initial={{ opacity: 0, y: 8 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: 8 }}
            className="fixed bottom-24 left-4 right-4 mx-auto max-w-md bg-slate-900 text-white px-4 py-2 rounded-xl shadow-lg text-sm text-center"
          >
            {toast}
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
