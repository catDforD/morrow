export type Role = 'system' | 'user' | 'assistant' | 'tool'

export type PermissionMode =
  | 'read_only'
  | 'workspace_write'
  | 'danger_full_access'

export type ShellPolicy = 'deny' | 'prompt' | 'allow'

export interface PermissionProfile {
  mode: PermissionMode
  shell: ShellPolicy
}

export interface StatusResponse {
  workspace_root: string
  config_path: string
  permissions: PermissionProfile
  version: string
}

export interface SessionEntryResponse {
  name: string
  path: string
  turns: number
  active_messages: number
  summarized_turns: number
  has_summary: boolean
}

export interface ToolFunctionCall {
  name: string
  arguments: string
}

export interface ToolCall {
  id: string
  type: 'function'
  function: ToolFunctionCall
}

export interface Message {
  role: Role
  content?: string | null
  tool_calls?: ToolCall[]
  tool_call_id?: string
}

export interface Thread {
  messages: Message[]
}

export interface SessionContext {
  summary?: string
  summarized_turns: number
}

export type TurnStatus = 'running' | 'completed' | 'failed'

export interface TurnStep {
  kind: 'model_call' | 'tool_call'
  status: TurnStatus
  tool_name?: string
  tool_call_id?: string
  error?: string | null
}

export interface Turn {
  status: TurnStatus
  user_message: Message
  assistant_message?: Message | null
  steps: TurnStep[]
  error?: string | null
}

export interface TurnRecord {
  turn: Turn
  messages: Message[]
}

export interface Session {
  active_thread: Thread
  turns: TurnRecord[]
  context: SessionContext
}

export interface SessionDocument {
  schema_version: number
  session: Session
}

export type FileChangeOperation = 'add' | 'update' | 'delete'

export interface FileChangeSummary {
  path: string
  operation: FileChangeOperation
  replacements: number
  created: boolean
  overwritten: boolean
  deleted: boolean
}

export interface ShellCommandSummary {
  command: string
  exit_code?: number | null
  timed_out: boolean
  stdout_truncated: boolean
  stderr_truncated: boolean
}

export interface ToolExecutionSummary {
  files?: FileChangeSummary[]
  diff?: string
  shell?: ShellCommandSummary
  error?: string
}

export type ApprovalAction =
  | {
      kind: 'shell_command'
      command: string
      cwd: string
      timeout_secs: number
    }
  | {
      kind: 'file_changes'
      files: FileChangeSummary[]
      diff: string
    }

export interface ApprovalRequest {
  id: string
  action: ApprovalAction
  reason: string
}

export interface ApprovalDecision {
  request_id: string
  approved: boolean
}

export type AgentEvent =
  | { type: 'turn_started' }
  | { type: 'warning'; data: string }
  | { type: 'text_delta'; data: string }
  | { type: 'agent_message'; data: string }
  | { type: 'tool_call_started'; data: { id: string; name: string } }
  | {
      type: 'tool_call_finished'
      data: {
        id: string
        name: string
        ok: boolean
        summary?: ToolExecutionSummary
      }
    }
  | { type: 'approval_requested'; data: ApprovalRequest }
  | { type: 'approval_resolved'; data: ApprovalDecision }
  | { type: 'turn_completed' }
  | { type: 'error'; data: string }

export interface AgentEventEnvelope {
  schema_version: number
  timestamp_ms: number
  session: string
  workspace_root: string
  turn_index: number
  event_index: number
  event: AgentEvent
}

export interface RunningTurnSnapshot {
  turn_id: string
  pending_approval?: string | null
}

export type ServerMessage =
  | {
      type: 'snapshot'
      data: {
        session: Session
        running_turn?: RunningTurnSnapshot | null
        permissions: PermissionProfile
      }
    }
  | { type: 'agent_event'; data: AgentEventEnvelope }
  | { type: 'turn_saved'; data: { session: string; turn_index: number } }
  | { type: 'turn_rejected'; data: { request_id: string; reason: string } }
  | { type: 'error'; data: { message: string } }

export type ClientMessage =
  | { type: 'start_turn'; data: { request_id: string; prompt: string } }
  | {
      type: 'approval_decision'
      data: { request_id: string; approved: boolean }
    }
  | { type: 'cancel_turn'; data: { turn_id: string } }

export interface ToolRun {
  id: string
  name: string
  status: 'running' | 'ok' | 'error'
  summary?: ToolExecutionSummary
}

export interface ActivityItem {
  id: string
  title: string
  detail?: string
  tone: 'neutral' | 'running' | 'ok' | 'error' | 'approval'
  time: string
}

export type TimelineMessageRole = 'user' | 'assistant'

export interface TimelineMessageItem {
  kind: 'message'
  id: string
  role: TimelineMessageRole
  content: string
}

export interface TimelineNoticeItem {
  kind: 'notice'
  id: string
  tone: ActivityItem['tone']
  title: string
  detail?: string
}

export type RunTraceStatus = 'running' | 'completed' | 'failed' | 'approval'
export type RunStepKind = 'model' | 'tool' | 'approval' | 'error' | 'final'
export type RunStepStatus = 'running' | 'ok' | 'error' | 'approval'

export interface RunStep {
  id: string
  kind: RunStepKind
  status: RunStepStatus
  title: string
  detail?: string
  summary?: ToolExecutionSummary
}

export interface RunTrace {
  id: string
  status: RunTraceStatus
  collapsed: boolean
  startedAt: string
  completedAt?: string
  steps: RunStep[]
  toolCount: number
}

export interface TimelineRunItem {
  kind: 'run'
  id: string
  trace: RunTrace
}

export type TimelineItem =
  | TimelineMessageItem
  | TimelineNoticeItem
  | TimelineRunItem
