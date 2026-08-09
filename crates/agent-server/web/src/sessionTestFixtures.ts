import type {
  ModelSelection,
  SessionEntryResponse,
  SessionSnapshot,
  SessionStreamFrame,
  SessionUpdate,
  TurnProjection,
} from './types'

export const testModelSelection: ModelSelection = {
  provider_id: 'provider-1',
  model_id: 'model-1',
  reasoning: 'off',
}

export function sessionEntry(
  name = 'task-one',
  archived = false,
): SessionEntryResponse {
  return {
    name,
    path: `/sessions/${name}.jsonl`,
    turns: 0,
    active_messages: 0,
    summarized_turns: 0,
    has_summary: false,
    archived,
  }
}

export function turnProjection(
  status: TurnProjection['status'] = 'running',
): TurnProjection {
  return {
    id: 'turn-1',
    operation_id: 'operation-1',
    index: 0,
    status,
    user_message: { role: 'user', content: 'hello' },
    model: {
      provider_id: 'provider-1',
      provider_name: 'Provider',
      model_id: 'model-1',
      model_name: 'Model',
      reasoning: 'off',
    },
    permissions: { mode: 'workspace_write', shell: 'prompt' },
    messages: [{ role: 'user', content: 'hello' }],
    steps: [],
    notices: [],
    started_at_ms: 1,
  }
}

export function sessionSnapshot(
  name = 'task-one',
  sequence = 0,
): SessionSnapshot {
  return {
    schema_version: 3,
    session_name: name,
    session_id: `session-${name}`,
    revision: 1,
    cursor: { stream_id: `stream-${name}`, sequence },
    session: {
      session_id: `session-${name}`,
      revision: 1,
      turns: [],
      context: { messages: [] },
      middleware_audit: [],
      diagnostics: [],
    },
    active_operation: null,
    permissions: { mode: 'workspace_write', shell: 'prompt' },
    approvals: [],
    subagents: [],
  }
}

export function snapshotFrame(
  name = 'task-one',
  sequence = 0,
): SessionStreamFrame {
  return { type: 'snapshot', data: sessionSnapshot(name, sequence) }
}

export function eventFrame(
  name: string,
  sequence: number,
  update: SessionUpdate,
): SessionStreamFrame {
  return {
    type: 'event',
    data: {
      schema_version: 3,
      stream_id: `stream-${name}`,
      sequence,
      session_revision: 1,
      timestamp_ms: sequence,
      update,
    },
  }
}
