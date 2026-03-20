import { Session, SessionDetail, WatchRepoRef, WatchRow, WatchSection, WatchSessionNode } from './types';

export type StatusFilter = 'all' | Session['status'];

const WAITING_STATES = new Set(['waiting', 'waiting_permission', 'waiting_input']);

function sortSessions(left: Session, right: Session): number {
  return sessionDisplayName(left).localeCompare(sessionDisplayName(right), undefined, { sensitivity: 'base' })
    || left.id.localeCompare(right.id);
}

export function sessionDisplayName(session: Session): string {
  return (session.friendly_name && session.friendly_name.trim()) || session.name || session.id;
}

export function repoKey(workingDir: string): string {
  return (workingDir || '').trim() || 'unknown';
}

export function repoLabel(workingDir: string): string {
  const normalized = repoKey(workingDir);
  const parts = normalized.split('/').filter(Boolean);
  return `${parts[parts.length - 1] || normalized || 'unknown'}/`;
}

export function parentLabel(session: Session, sessionsById: Map<string, Session>): string {
  const parentId = session.parent_session_id?.trim();
  if (!parentId) {
    return '-';
  }
  const parent = sessionsById.get(parentId);
  if (!parent) {
    return parentId;
  }
  const name = sessionDisplayName(parent);
  return name === parentId ? parentId : `${name} [${parentId}]`;
}

export function parseIso(value?: string | null): Date | null {
  if (!value) {
    return null;
  }
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

function elapsedLabel(seconds: number): string {
  if (seconds < 60) {
    return `${seconds}s`;
  }
  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m`;
  }
  if (seconds < 86400) {
    return `${Math.floor(seconds / 3600)}h`;
  }
  return `${Math.floor(seconds / 86400)}d`;
}

export function ageFromIso(value?: string | null): string {
  const parsed = parseIso(value);
  if (!parsed) {
    return '-';
  }
  const seconds = Math.max(0, Math.floor((Date.now() - parsed.getTime()) / 1000));
  return elapsedLabel(seconds);
}

export function formatAge(lastActivity?: string | null, activityState?: string | null): string {
  const parsed = parseIso(lastActivity);
  if (!parsed) {
    return '-';
  }
  const seconds = Math.max(0, Math.floor((Date.now() - parsed.getTime()) / 1000));
  if (activityState === 'working' || activityState === 'thinking') {
    return `${seconds}s`;
  }
  return `${Math.floor(seconds / 60)}m`;
}

export function formatDateTime(value?: string | null): string {
  const parsed = parseIso(value);
  if (!parsed) {
    return value || '-';
  }
  return parsed.toLocaleString([], {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

export function currentThinkingDuration(session: Session): string {
  const provider = session.provider || 'claude';
  if (provider === 'codex-app') {
    return ageFromIso(session.last_action_at || session.last_activity);
  }
  if (provider === 'claude') {
    return ageFromIso(session.last_tool_call || session.last_activity);
  }
  return ageFromIso(session.last_activity);
}

export function lastColumn(session: Session): string {
  const provider = session.provider || 'claude';
  if (provider === 'codex') {
    return 'n/a (no hooks)';
  }
  if (provider === 'codex-app') {
    const summary = session.last_action_summary;
    const at = session.last_action_at;
    if (summary && at) {
      return `${summary} (${ageFromIso(at)})`;
    }
    return summary || '-';
  }
  const toolName = session.last_tool_name;
  const toolAt = session.last_tool_call;
  if (toolName && toolAt) {
    return `${toolName} (${ageFromIso(toolAt)})`;
  }
  if (toolName) {
    return toolName;
  }
  if (toolAt) {
    return `tool (${ageFromIso(toolAt)})`;
  }
  return '-';
}

export function stateLabel(activityState?: string | null): string {
  if (!activityState) {
    return 'idle';
  }
  if (WAITING_STATES.has(activityState)) {
    return 'waiting';
  }
  return activityState;
}

export function statusLines(session: Session): string[] {
  const lines: string[] = [];
  if (session.agent_status_text) {
    const ageSuffix = session.agent_status_at ? ` (${ageFromIso(session.agent_status_at)})` : '';
    lines.push(`status: "${session.agent_status_text}"${ageSuffix}`);
  }
  if (session.agent_task_completed_at) {
    lines.push(`task: completed (${ageFromIso(session.agent_task_completed_at)})`);
  }
  for (const proposal of session.pending_adoption_proposals || []) {
    if ((proposal.status || 'pending') !== 'pending') {
      continue;
    }
    const proposerName = proposal.proposer_name || proposal.proposer_session_id || 'unknown';
    const proposerId = proposal.proposer_session_id || 'unknown';
    const age = ageFromIso(proposal.created_at);
    const ageSuffix = age !== '-' ? ` (${age})` : '';
    lines.push(`adopt: pending from ${proposerName} [${proposerId}]${ageSuffix}`);
  }
  return lines;
}

export function detailLines(session: Session, detail?: SessionDetail | null): string[] {
  const lines = [
    `meta: ${sessionDisplayName(session)} [${session.id}] provider=${session.provider || 'claude'} activity=${stateLabel(session.activity_state)} status=${session.status || '-'} role=${session.role || '-'}${session.is_em ? ' em=yes' : ''}${session.is_maintainer ? ' maintainer=yes' : ''}`,
    `thinking duration: ${currentThinkingDuration(session)}`,
    `context size: ${session.context_monitor_enabled ? `${Number(session.tokens_used || 0).toLocaleString()} tokens` : 'n/a (monitor off)'}`,
    `working dir: ${session.working_dir || '-'}`,
    `tmux: ${session.tmux_session || '-'}`,
    `git remote: ${session.git_remote_url || 'N/A'}`,
    `aliases: ${(session.aliases && session.aliases.length > 0) ? session.aliases.join(', ') : '-'}`,
    `current task: ${session.current_task || 'No current task'}`,
    'last 10 tool calls/actions:',
  ];

  if (!detail || detail.loading) {
    lines.push('  loading...');
  } else {
    if (detail.last_error) {
      lines.push(`  warning: ${detail.last_error}`);
    }
    if (detail.action_lines.length === 0) {
      lines.push('  -');
    } else {
      for (const entry of detail.action_lines.slice(0, 10)) {
        lines.push(`  ${entry}`);
      }
    }
  }

  lines.push('last 10 tail lines:');
  if (!detail || detail.loading) {
    lines.push('  loading...');
  } else if (detail.tail_lines.length === 0) {
    lines.push('  -');
  } else {
    for (const entry of detail.tail_lines.slice(0, 10)) {
      lines.push(`  ${entry}`);
    }
  }

  return lines;
}

export function buildWatchSections(sessions: Session[]): WatchSection[] {
  const sessionsById = new Map(sessions.map((session) => [session.id, session]));
  const rootsByRepo = new Map<string, Session[]>();
  const sameRepoChildren = new Map<string, Session[]>();
  const crossRepoChildren = new Map<string, Map<string, Session[]>>();
  const repoKeys = new Set<string>();

  for (const session of sessions) {
    const key = repoKey(session.working_dir);
    repoKeys.add(key);
    const parentId = session.parent_session_id?.trim();
    if (!parentId) {
      rootsByRepo.set(key, [...(rootsByRepo.get(key) || []), session]);
      continue;
    }

    const parent = sessionsById.get(parentId);
    if (!parent) {
      rootsByRepo.set(key, [...(rootsByRepo.get(key) || []), session]);
      continue;
    }

    const parentRepo = repoKey(parent.working_dir);
    if (parentRepo === key) {
      sameRepoChildren.set(parentId, [...(sameRepoChildren.get(parentId) || []), session]);
      continue;
    }

    const repoMap = new Map(crossRepoChildren.get(parentId) || []);
    repoMap.set(key, [...(repoMap.get(key) || []), session]);
    crossRepoChildren.set(parentId, repoMap);
  }

  const buildNode = (session: Session): WatchSessionNode => {
    const localChildren = [...(sameRepoChildren.get(session.id) || [])].sort(sortSessions).map(buildNode);
    const remoteGroups = [...(crossRepoChildren.get(session.id)?.entries() || [])]
      .sort(([left], [right]) => repoLabel(left).localeCompare(repoLabel(right), undefined, { sensitivity: 'base' }) || left.localeCompare(right))
      .map(([key, children]): WatchRepoRef => ({
        repoKey: key,
        repoLabel: repoLabel(key),
        children: [...children].sort(sortSessions).map(buildNode),
      }));

    return {
      session,
      sameRepoChildren: localChildren,
      crossRepoGroups: remoteGroups,
    };
  };

  return [...repoKeys]
    .sort((left, right) => repoLabel(left).localeCompare(repoLabel(right), undefined, { sensitivity: 'base' }) || left.localeCompare(right))
    .map((key): WatchSection => ({
      repoKey: key,
      repoLabel: repoLabel(key),
      roots: [...(rootsByRepo.get(key) || [])].sort(sortSessions).map(buildNode),
    }))
    .filter((section) => section.roots.length > 0);
}

function matchesSearch(session: Session, query: string): boolean {
  if (!query) {
    return true;
  }
  const haystack = [
    session.id,
    session.name,
    sessionDisplayName(session),
    session.tmux_session,
    session.working_dir,
    session.role || '',
    session.provider || '',
    ...(session.aliases || []),
  ]
    .join(' ')
    .toLowerCase();
  return haystack.includes(query);
}

function matchesStatus(session: Session, filter: StatusFilter): boolean {
  return filter === 'all' || session.status === filter;
}

function filterNode(node: WatchSessionNode, filter: StatusFilter, query: string): WatchSessionNode | null {
  const sameRepoChildren = node.sameRepoChildren
    .map((child) => filterNode(child, filter, query))
    .filter((child): child is WatchSessionNode => child !== null);

  const crossRepoGroups = node.crossRepoGroups
    .map((group) => ({
      ...group,
      children: group.children
        .map((child) => filterNode(child, filter, query))
        .filter((child): child is WatchSessionNode => child !== null),
    }))
    .filter((group) => group.children.length > 0);

  if (matchesSearch(node.session, query) && matchesStatus(node.session, filter)) {
    return {
      ...node,
      sameRepoChildren,
      crossRepoGroups,
    };
  }

  if (sameRepoChildren.length > 0 || crossRepoGroups.length > 0) {
    return {
      ...node,
      sameRepoChildren,
      crossRepoGroups,
    };
  }

  return null;
}

export function filterWatchSections(
  sections: WatchSection[],
  filter: StatusFilter,
  query: string,
): WatchSection[] {
  const normalizedQuery = query.trim().toLowerCase();
  return sections
    .map((section) => ({
      ...section,
      roots: section.roots
        .map((node) => filterNode(node, filter, normalizedQuery))
        .filter((node): node is WatchSessionNode => node !== null),
    }))
    .filter((section) => section.roots.length > 0);
}

export function statsFromSessions(sessions: Session[]) {
  return {
    total: sessions.length,
    running: sessions.filter((session) => session.status === 'running').length,
    idle: sessions.filter((session) => session.status === 'idle').length,
    stopped: sessions.filter((session) => session.status === 'stopped').length,
    em: sessions.filter((session) => Boolean(session.is_em)).length,
    maintainers: sessions.filter((session) => Boolean(session.is_maintainer)).length,
    thinking: sessions.filter((session) => session.activity_state === 'thinking').length,
    working: sessions.filter((session) => session.activity_state === 'working').length,
  };
}

export function buildWatchRows(
  sections: WatchSection[],
  sessionsById: Map<string, Session>,
  expanded: Set<string>,
  detailsById: Record<string, SessionDetail | undefined>,
): WatchRow[] {
  const rows: WatchRow[] = [];

  const pushSession = (node: WatchSessionNode, depth: number) => {
    const session = node.session;
    rows.push({
      kind: 'session',
      id: `session-${session.id}`,
      session,
      depth,
      activityState: stateLabel(session.activity_state),
      columns: {
        Session: sessionDisplayName(session),
        ID: session.id,
        Parent: parentLabel(session, sessionsById),
        Role: session.role || (session.is_em ? 'em' : '-'),
        Provider: session.provider || 'claude',
        Activity: stateLabel(session.activity_state),
        Status: session.status || '-',
        Last: lastColumn(session),
        Age: formatAge(session.last_activity, session.activity_state),
      },
    });

    for (const line of statusLines(session)) {
      rows.push({
        kind: 'status',
        id: `status-${session.id}-${line}`,
        session,
        depth: depth + 1,
        text: line,
      });
    }

    if (expanded.has(session.id)) {
      rows.push({
        kind: 'detail',
        id: `detail-${session.id}`,
        session,
        depth: depth + 1,
        detailLines: detailLines(session, detailsById[session.id]),
      });
    }

    const childEntries: Array<{ kind: 'session' | 'repo-ref'; key: string; payload: WatchSessionNode | WatchRepoRef }> = [];
    for (const child of node.sameRepoChildren) {
      childEntries.push({ kind: 'session', key: sessionDisplayName(child.session).toLowerCase(), payload: child });
    }
    for (const group of node.crossRepoGroups) {
      childEntries.push({ kind: 'repo-ref', key: group.repoLabel.toLowerCase(), payload: group });
    }
    childEntries.sort((left, right) => left.key.localeCompare(right.key) || left.kind.localeCompare(right.kind));

    for (const entry of childEntries) {
      if (entry.kind === 'session') {
        pushSession(entry.payload as WatchSessionNode, depth + 1);
      } else {
        pushRepoRef(entry.payload as WatchRepoRef, depth + 1, session.id);
      }
    }
  };

  const pushRepoRef = (group: WatchRepoRef, depth: number, parentSessionId: string) => {
    rows.push({
      kind: 'repo-ref',
      id: `repo-ref-${parentSessionId}-${group.repoKey}`,
      depth,
      text: `${group.repoLabel} (${group.repoKey})`,
    });
    for (const child of group.children) {
      pushSession(child, depth + 1);
    }
  };

  for (const section of sections) {
    rows.push({
      kind: 'repo',
      id: `repo-${section.repoKey}`,
      depth: 0,
      text: `${section.repoLabel} (${section.repoKey})`,
    });
    for (const root of section.roots) {
      pushSession(root, 0);
    }
  }

  return rows;
}
