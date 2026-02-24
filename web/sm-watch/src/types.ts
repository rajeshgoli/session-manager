export type SessionStatus = "running" | "idle" | "stopped";
export type ActivityState = "working" | "thinking" | "idle" | "waiting" | "stopped";

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
}
