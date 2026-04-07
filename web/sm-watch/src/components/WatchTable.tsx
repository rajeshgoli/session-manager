import React from 'react';
import {
  Bug,
  ChevronDown,
  ChevronRight,
  Copy,
  ExternalLink,
  ShieldAlert,
  Trash2,
} from 'lucide-react';
import { WatchRow, Session } from '../types';

interface WatchTableProps {
  rows: WatchRow[];
  expandedSessions: Set<string>;
  isOpeningTelegram: string | null;
  copiedAttachSessionId: string | null;
  onToggleExpand: (sessionId: string) => void;
  onOpenTelegram: (session: Session) => void;
  onKillSession: (id: string, event: React.MouseEvent) => Promise<void>;
  onCopyAttach: (session: Session) => Promise<void>;
  onReportBug: (session: Session) => void;
}

const GRID_CLASS =
  'grid min-w-[1180px] grid-cols-[minmax(18rem,2.5fr)_minmax(6rem,0.8fr)_minmax(12rem,1.5fr)_minmax(7rem,0.8fr)_minmax(8rem,0.9fr)_minmax(8rem,0.9fr)_minmax(7rem,0.8fr)_minmax(15rem,2fr)_minmax(4rem,0.55fr)] items-center gap-x-3';

function normalizeChatId(chatId?: number | null): string {
  if (!chatId) {
    return '';
  }
  return String(Math.abs(chatId)).replace(/^100/, '');
}

function telegramDeepLink(chatId?: number | null, threadId?: number | null): string | null {
  if (!chatId) {
    return null;
  }
  const normalized = normalizeChatId(chatId);
  return threadId ? `https://t.me/c/${normalized}/${threadId}` : `https://t.me/c/${normalized}`;
}

function attachCommand(session: Session): string | null {
  const attach = session.termux_attach;
  if (!attach?.supported) {
    return null;
  }
  if (attach.ssh_command) {
    return attach.ssh_command;
  }
  if (!attach.ssh_host || !attach.ssh_username || !attach.tmux_session) {
    return null;
  }
  const target = `${attach.ssh_username}@${attach.ssh_host}`;
  const quotedTmux = attach.tmux_session.replace(/'/g, `'"'"'`);
  return `ssh -t ${target} 'tmux attach-session -t ${quotedTmux}'`;
}

function activityTone(activityState?: string): string {
  switch (activityState) {
    case 'working':
      return 'text-emerald-300';
    case 'thinking':
      return 'text-sky-300';
    case 'waiting':
    case 'waiting_input':
    case 'waiting_permission':
      return 'text-amber-300';
    case 'stopped':
      return 'text-rose-300';
    default:
      return 'text-zinc-400';
  }
}

function statusTone(status?: string): string {
  switch (status) {
    case 'running':
      return 'border-emerald-500/30 bg-emerald-500/10 text-emerald-200';
    case 'stopped':
      return 'border-rose-500/30 bg-rose-500/10 text-rose-200';
    default:
      return 'border-zinc-700 bg-zinc-800 text-zinc-200';
  }
}

function providerTone(provider?: string): string {
  switch (provider) {
    case 'codex-fork':
      return 'text-cyan-300';
    case 'claude':
      return 'text-fuchsia-300';
    case 'codex-app':
      return 'text-violet-300';
    default:
      return 'text-zinc-300';
  }
}

function RowLabel({ depth, children }: { depth: number; children: React.ReactNode }) {
  return <div style={{ paddingLeft: `${depth * 18}px` }}>{children}</div>;
}

function SessionDetailBlock({
  session,
  detailLines,
  copiedAttachSessionId,
  isOpeningTelegram,
  onOpenTelegram,
  onKillSession,
  onCopyAttach,
  onReportBug,
}: {
  session: Session;
  detailLines: string[];
  copiedAttachSessionId: string | null;
  isOpeningTelegram: string | null;
  onOpenTelegram: (session: Session) => void;
  onKillSession: (id: string, event: React.MouseEvent) => Promise<void>;
  onCopyAttach: (session: Session) => Promise<void>;
  onReportBug: (session: Session) => void;
}) {
  const telegramLink = telegramDeepLink(session.telegram_chat_id, session.telegram_thread_id);
  const attach = attachCommand(session);

  return (
    <div className="rounded-2xl border border-zinc-800 bg-zinc-950/90 px-4 py-4">
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={() => onOpenTelegram(session)}
          disabled={!telegramLink}
          className="inline-flex items-center gap-2 rounded-full border border-zinc-700 bg-zinc-900 px-3 py-1.5 text-[11px] font-semibold uppercase tracking-[0.18em] text-zinc-100 transition hover:border-cyan-400/50 hover:text-cyan-200 disabled:cursor-not-allowed disabled:text-zinc-500"
        >
          <ExternalLink size={13} />
          {isOpeningTelegram === session.id ? 'Opening thread' : telegramLink ? 'Open thread' : 'No thread'}
        </button>
        <button
          type="button"
          onClick={() => void onCopyAttach(session)}
          disabled={!attach}
          className="inline-flex items-center gap-2 rounded-full border border-zinc-700 bg-zinc-900 px-3 py-1.5 text-[11px] font-semibold uppercase tracking-[0.18em] text-zinc-100 transition hover:border-emerald-400/50 hover:text-emerald-200 disabled:cursor-not-allowed disabled:text-zinc-500"
        >
          <Copy size={13} />
          {copiedAttachSessionId === session.id ? 'Attach copied' : attach ? 'Copy attach' : 'Attach unavailable'}
        </button>
        <button
          type="button"
          onClick={(event) => void onKillSession(session.id, event)}
          className="inline-flex items-center gap-2 rounded-full border border-rose-500/30 bg-rose-500/10 px-3 py-1.5 text-[11px] font-semibold uppercase tracking-[0.18em] text-rose-200 transition hover:border-rose-400/60"
        >
          <Trash2 size={13} />
          Kill session
        </button>
        <button
          type="button"
          onClick={() => onReportBug(session)}
          className="inline-flex items-center gap-2 rounded-full border border-amber-500/30 bg-amber-500/10 px-3 py-1.5 text-[11px] font-semibold uppercase tracking-[0.18em] text-amber-200 transition hover:border-amber-400/60"
        >
          <Bug size={13} />
          Report bug
        </button>
        {session.is_maintainer && (
          <span className="inline-flex items-center gap-2 rounded-full border border-cyan-500/30 bg-cyan-500/10 px-3 py-1.5 text-[11px] font-semibold uppercase tracking-[0.18em] text-cyan-200">
            <ShieldAlert size={13} />
            Maintainer
          </span>
        )}
      </div>

      <div className="overflow-hidden rounded-xl border border-zinc-800 bg-black/40">
        <pre className="max-h-[24rem] overflow-auto px-4 py-3 font-mono text-[11px] leading-5 text-zinc-300 whitespace-pre-wrap">
          {detailLines.join('\n')}
        </pre>
      </div>
    </div>
  );
}

export function WatchTable({
  rows,
  expandedSessions,
  isOpeningTelegram,
  copiedAttachSessionId,
  onToggleExpand,
  onOpenTelegram,
  onKillSession,
  onCopyAttach,
  onReportBug,
}: WatchTableProps) {
  return (
    <div className="overflow-hidden rounded-[28px] border border-zinc-800 bg-zinc-950/90 shadow-[0_24px_80px_rgba(0,0,0,0.45)]">
      <div className="overflow-x-auto">
        <div className="border-b border-zinc-800 bg-zinc-950/95 px-4 py-3">
          <div className={`${GRID_CLASS} text-[11px] font-semibold uppercase tracking-[0.24em] text-zinc-500`}>
            <div>Session</div>
            <div>ID</div>
            <div>Parent</div>
            <div>Role</div>
            <div>Provider</div>
            <div>Activity</div>
            <div>Status</div>
            <div>Last</div>
            <div className="text-right">Age</div>
          </div>
        </div>

        <div className="divide-y divide-zinc-900/90">
          {rows.map((row) => {
            if (row.kind === 'repo') {
              return (
                <div key={row.id} className="border-t border-zinc-800 bg-zinc-900/85 px-4 py-2.5 first:border-t-0">
                  <div className="text-[11px] font-semibold uppercase tracking-[0.26em] text-cyan-300">
                    {row.text}
                  </div>
                </div>
              );
            }

            if (row.kind === 'repo-ref') {
              return (
                <div key={row.id} className="bg-zinc-950 px-4 py-2 text-[11px] text-zinc-500">
                  <RowLabel depth={row.depth}>
                    <span className="font-mono uppercase tracking-[0.22em] text-zinc-500">{row.text}</span>
                  </RowLabel>
                </div>
              );
            }

            if (row.kind === 'status' || row.kind === 'detail') {
              return (
                <div key={row.id} className={`px-4 ${row.kind === 'detail' ? 'py-0' : 'py-2'} bg-zinc-950/70`}>
                  <RowLabel depth={row.depth}>
                    {row.kind === 'status' ? (
                      <div className="font-mono text-[11px] leading-5 text-zinc-400">{row.text}</div>
                    ) : row.session ? (
                      <SessionDetailBlock
                        session={row.session}
                        detailLines={row.detailLines || []}
                        copiedAttachSessionId={copiedAttachSessionId}
                        isOpeningTelegram={isOpeningTelegram}
                        onOpenTelegram={onOpenTelegram}
                        onKillSession={onKillSession}
                        onCopyAttach={onCopyAttach}
                        onReportBug={onReportBug}
                      />
                    ) : null}
                  </RowLabel>
                </div>
              );
            }

            const session = row.session;
            if (!session || !row.columns) {
              return null;
            }
            const isExpanded = expandedSessions.has(session.id);

            return (
              <button
                key={row.id}
                type="button"
                onClick={() => onToggleExpand(session.id)}
                className="block w-full px-4 py-3 text-left transition hover:bg-zinc-900/70"
              >
                <div className={`${GRID_CLASS} text-sm text-zinc-100`}>
                  <RowLabel depth={row.depth}>
                    <div className="flex items-center gap-2">
                      <span className="text-zinc-500">{isExpanded ? <ChevronDown size={15} /> : <ChevronRight size={15} />}</span>
                      <div className="min-w-0">
                        <div className="flex items-center gap-2">
                          <span className="truncate font-semibold tracking-[-0.01em] text-zinc-50">{row.columns.Session}</span>
                          {session.aliases && session.aliases.length > 0 ? (
                            <span className="rounded-full border border-zinc-700 bg-zinc-900 px-2 py-0.5 font-mono text-[10px] uppercase tracking-[0.18em] text-zinc-400">
                              {session.aliases[0]}
                            </span>
                          ) : null}
                        </div>
                        <div className="mt-1 truncate font-mono text-[11px] text-zinc-500">{session.tmux_session}</div>
                      </div>
                    </div>
                  </RowLabel>
                  <div className="font-mono text-[12px] text-zinc-300">{row.columns.ID}</div>
                  <div className="truncate text-[12px] text-zinc-400">{row.columns.Parent}</div>
                  <div className="truncate text-[12px] text-zinc-300">{row.columns.Role}</div>
                  <div className={`truncate text-[12px] font-medium ${providerTone(row.columns.Provider)}`}>{row.columns.Provider}</div>
                  <div className={`truncate text-[12px] font-semibold uppercase tracking-[0.16em] ${activityTone(row.activityState)}`}>
                    {row.columns.Activity}
                  </div>
                  <div>
                    <span className={`inline-flex rounded-full border px-2 py-1 text-[10px] font-semibold uppercase tracking-[0.18em] ${statusTone(row.columns.Status)}`}>
                      {row.columns.Status}
                    </span>
                  </div>
                  <div className="truncate font-mono text-[12px] text-zinc-400">{row.columns.Last}</div>
                  <div className="text-right font-mono text-[12px] text-zinc-300">{row.columns.Age}</div>
                </div>
              </button>
            );
          })}
        </div>
      </div>
    </div>
  );
}
