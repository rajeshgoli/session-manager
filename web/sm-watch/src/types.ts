export type SessionStatus = 'running' | 'idle' | 'stopped';
export type ActivityState =
  | 'working'
  | 'thinking'
  | 'idle'
  | 'waiting'
  | 'waiting_permission'
  | 'waiting_input'
  | 'stopped';

export interface AdoptionProposal {
  proposer_session_id?: string | null;
  proposer_name?: string | null;
  created_at?: string | null;
  status?: string | null;
}

export interface ToolCallRow {
  timestamp?: string | null;
  tool_name?: string | null;
}

export interface ActivityActionRow {
  summary_text?: string | null;
  action_kind?: string | null;
  status?: string | null;
  started_at?: string | null;
  ended_at?: string | null;
}

export interface SessionDetail {
  action_lines: string[];
  tail_lines: string[];
  fetched_at: number;
  loading: boolean;
  last_error?: string | null;
}

export interface AttachDescriptor {
  attach_supported?: boolean;
  message?: string | null;
  tmux_session?: string | null;
  runtime_mode?: string | null;
}

export interface TermuxAttach {
  supported?: boolean;
  reason?: string | null;
  transport?: string | null;
  ssh_host?: string | null;
  ssh_username?: string | null;
  tmux_session?: string | null;
  runtime_mode?: string | null;
  termux_package?: string | null;
}

export interface PrimaryAction {
  type?: string | null;
  label?: string | null;
  reason?: string | null;
}

export interface Session {
  id: string;
  name: string;
  working_dir: string;
  status: SessionStatus;
  created_at: string;
  last_activity: string;
  tmux_session: string;
  provider?: string;
  friendly_name?: string | null;
  telegram_chat_id?: number | null;
  telegram_thread_id?: number | null;
  current_task?: string | null;
  git_remote_url?: string | null;
  parent_session_id?: string | null;
  last_handoff_path?: string | null;
  agent_status_text?: string | null;
  agent_status_at?: string | null;
  agent_task_completed_at?: string | null;
  is_em?: boolean;
  role?: string | null;
  activity_state?: ActivityState;
  last_tool_call?: string | null;
  last_tool_name?: string | null;
  last_action_summary?: string | null;
  last_action_at?: string | null;
  tokens_used?: number;
  context_monitor_enabled?: boolean;
  pending_adoption_proposals?: AdoptionProposal[];
  aliases?: string[];
  is_maintainer?: boolean;
  attach_descriptor?: AttachDescriptor | null;
  termux_attach?: TermuxAttach | null;
  primary_action?: PrimaryAction | null;
}

export interface WatchSection {
  repoKey: string;
  repoLabel: string;
  roots: WatchSessionNode[];
}

export interface WatchSessionNode {
  session: Session;
  sameRepoChildren: WatchSessionNode[];
  crossRepoGroups: WatchRepoRef[];
}

export interface WatchRepoRef {
  repoKey: string;
  repoLabel: string;
  children: WatchSessionNode[];
}

export interface WatchRow {
  kind: 'repo' | 'session' | 'status' | 'detail' | 'repo-ref';
  id: string;
  session?: Session;
  depth: number;
  columns?: Record<string, string>;
  text?: string;
  detailLines?: string[];
  activityState?: string;
}
