import type {
  RunStep,
  RunTrace,
  SessionSnapshot,
  SessionStepProjection,
  SessionStreamFrame,
  SessionUpdateEnvelope,
  TimelineItem,
  ToolRun,
  TurnProjection,
} from './types'

export const SESSION_STREAM_SCHEMA_VERSION = 2

export interface SessionTimelineState {
  snapshot: SessionSnapshot | null
  resyncRequired: boolean
  resyncReason?: string
  lastNotice?: string
}

export function emptySessionTimelineState(): SessionTimelineState {
  return { snapshot: null, resyncRequired: false }
}

export function reduceSessionFrame(
  state: SessionTimelineState,
  frame: SessionStreamFrame,
): SessionTimelineState {
  switch (frame.type) {
    case 'snapshot':
      if (frame.data.schema_version !== SESSION_STREAM_SCHEMA_VERSION) {
        return requireResync(
          state,
          `unsupported session stream v${frame.data.schema_version}`,
        )
      }
      return {
        snapshot: frame.data,
        resyncRequired: false,
      }
    case 'event':
      return applyEvent(state, frame.data)
    case 'resync_required':
      return requireResync(state, frame.data.reason)
    case 'command_result':
    case 'command_data':
      return state
  }
}

function applyEvent(
  state: SessionTimelineState,
  envelope: SessionUpdateEnvelope,
): SessionTimelineState {
  const current = state.snapshot
  if (!current) return requireResync(state, 'event received before snapshot')
  if (envelope.schema_version !== SESSION_STREAM_SCHEMA_VERSION) {
    return requireResync(
      state,
      `unsupported session event v${envelope.schema_version}`,
    )
  }
  if (envelope.stream_id !== current.cursor.stream_id) {
    return requireResync(state, 'session stream epoch changed')
  }
  if (envelope.sequence <= current.cursor.sequence) return state
  if (envelope.sequence !== current.cursor.sequence + 1) {
    return requireResync(
      state,
      `session stream sequence gap: expected ${current.cursor.sequence + 1}, received ${envelope.sequence}`,
    )
  }

  const snapshot: SessionSnapshot = {
    ...current,
    revision: envelope.session_revision,
    cursor: { ...current.cursor, sequence: envelope.sequence },
    session: {
      ...current.session,
      revision: envelope.session_revision,
      turns: [...current.session.turns],
    },
    approvals: [...current.approvals],
    subagents: [...current.subagents],
  }
  let lastNotice = state.lastNotice

  switch (envelope.update.type) {
    case 'turn_upserted': {
      const turn = envelope.update.data
      const index = snapshot.session.turns.findIndex(
        (currentTurn) => currentTurn.id === turn.id,
      )
      if (index >= 0) snapshot.session.turns[index] = turn
      else snapshot.session.turns.push(turn)
      snapshot.session.turns.sort((left, right) => left.index - right.index)
      break
    }
    case 'context_replaced':
      snapshot.session.context = envelope.update.data
      break
    case 'operation_replaced':
      snapshot.active_operation = envelope.update.data
      break
    case 'model_stream_delta': {
      const delta = envelope.update.data
      const operation = snapshot.active_operation
      if (!operation || operation.operation_id !== delta.operation_id) {
        return requireResync(state, 'stream delta does not match active operation')
      }
      const streaming =
        operation.streaming?.model_call_id === delta.model_call_id
          ? { ...operation.streaming }
          : {
              model_call_id: delta.model_call_id,
              content: '',
              reasoning: '',
            }
      streaming.content += delta.text ?? ''
      streaming.reasoning += delta.reasoning ?? ''
      snapshot.active_operation = { ...operation, streaming }
      break
    }
    case 'approvals_replaced':
      snapshot.approvals = envelope.update.data
      break
    case 'subagent_upserted': {
      const subagent = envelope.update.data
      snapshot.subagents = snapshot.subagents.filter(
        (currentSubagent) => currentSubagent.id !== subagent.id,
      )
      snapshot.subagents.push(subagent)
      snapshot.subagents.sort(
        (left, right) => left.created_at_ms - right.created_at_ms,
      )
      break
    }
    case 'subagent_removed': {
      const instanceId = envelope.update.data.instance_id
      snapshot.subagents = snapshot.subagents.filter(
        (subagent) => subagent.id !== instanceId,
      )
      break
    }
    case 'notice':
      lastNotice = envelope.update.data.message
      break
  }

  return {
    snapshot,
    resyncRequired: false,
    ...(lastNotice ? { lastNotice } : {}),
  }
}

function requireResync(
  state: SessionTimelineState,
  reason: string,
): SessionTimelineState {
  return { ...state, resyncRequired: true, resyncReason: reason }
}

export function timelineFromSnapshot(
  snapshot: SessionSnapshot | null,
): TimelineItem[] {
  if (!snapshot) return []
  const items = snapshot.session.turns.flatMap((turn) => turnTimeline(turn, snapshot))
  for (const diagnostic of snapshot.session.diagnostics) {
    items.unshift({
      kind: 'notice',
      id: `diagnostic-${diagnostic}`,
      tone: 'neutral',
      title: 'Session migration notice',
      detail: diagnostic,
    })
  }
  return items
}

function turnTimeline(
  turn: TurnProjection,
  snapshot: SessionSnapshot,
): TimelineItem[] {
  const items: TimelineItem[] = []
  if (turn.user_message.content) {
    items.push({
      kind: 'message',
      id: `${turn.id}-user`,
      role: 'user',
      content: turn.user_message.content,
    })
  }

  for (const [index, notice] of turn.notices.entries()) {
    items.push({
      kind: 'notice',
      id: `${turn.id}-notice-${index}`,
      tone: 'neutral',
      title: 'Notice',
      detail: notice,
    })
  }

  if (turn.steps.length > 0 || turn.error) {
    const trace = projectionRunTrace(turn, snapshot)
    items.push({ kind: 'run', id: trace.id, trace })
  }

  const assistant = [...turn.messages]
    .reverse()
    .find(
      (message) =>
        message.role === 'assistant' &&
        Boolean(message.content?.trim()) &&
        !message.tool_calls?.length,
    )
  if (assistant?.content) {
    items.push({
      kind: 'message',
      id: `${turn.id}-assistant`,
      role: 'assistant',
      content: assistant.content,
    })
  }

  const operation = snapshot.active_operation
  if (
    operation?.turn_id === turn.id &&
    operation.streaming?.content &&
    operation.streaming.content !== assistant?.content
  ) {
    items.push({
      kind: 'message',
      id: `${turn.id}-${operation.streaming.model_call_id}-streaming`,
      role: 'assistant',
      content: operation.streaming.content,
    })
  }
  return items
}

function projectionRunTrace(
  turn: TurnProjection,
  snapshot: SessionSnapshot,
): RunTrace {
  const steps = turn.steps.map((step) => projectionRunStep(turn, step, snapshot))
  if (turn.error && !steps.some((step) => step.status === 'error')) {
    steps.push({
      id: `${turn.id}-error`,
      kind: 'error',
      status: 'error',
      title: 'Error',
      detail: turn.error,
    })
  }
  return {
    id: `${turn.id}-run`,
    status:
      turn.status === 'completed'
        ? 'completed'
        : turn.status === 'running'
          ? steps.some((step) => step.status === 'approval')
            ? 'approval'
            : 'running'
          : 'failed',
    collapsed: turn.status !== 'running',
    startedAt: `turn ${turn.index + 1}`,
    ...(turn.completed_at_ms
      ? { completedAt: new Date(turn.completed_at_ms).toLocaleTimeString() }
      : {}),
    steps,
    toolCount: steps.filter((step) => step.kind === 'tool').length,
  }
}

function projectionRunStep(
  turn: TurnProjection,
  step: SessionStepProjection,
  snapshot: SessionSnapshot,
): RunStep {
  const pendingApproval = step.approval && !step.approval_decision
  const status = pendingApproval
    ? 'approval'
    : step.status === 'completed'
      ? 'ok'
      : step.status === 'running'
        ? 'running'
        : 'error'
  const streaming =
    snapshot.active_operation?.turn_id === turn.id &&
    snapshot.active_operation.streaming?.model_call_id === step.id
      ? snapshot.active_operation.streaming
      : undefined
  if (step.kind === 'model_call') {
    return {
      id: `${turn.id}-${step.id}`,
      kind: 'model',
      status,
      title: turn.model.model_name || 'Model call',
      detail: step.error || `${turn.model.provider_name} · ${turn.model.reasoning}`,
      reasoning:
        streaming?.reasoning || step.model_message?.reasoning_content || undefined,
    }
  }
  return {
    id: `${turn.id}-${step.id}`,
    kind: pendingApproval ? 'approval' : 'tool',
    status,
    title: pendingApproval
      ? 'Waiting for approval'
      : step.tool_call?.function.name || 'Tool call',
    detail:
      step.error ||
      (step.status === 'outcome_unknown'
        ? 'Tool side effect may have completed; outcome is unknown after recovery.'
        : step.tool_call?.id),
    summary: step.tool_summary || undefined,
  }
}

export function toolsFromSnapshot(snapshot: SessionSnapshot | null): ToolRun[] {
  const operation = snapshot?.active_operation
  if (!snapshot || !operation) return []
  const turn = snapshot.session.turns.find((turn) => turn.id === operation.turn_id)
  if (!turn) return []
  return turn.steps.flatMap((step): ToolRun[] => {
    if (step.kind !== 'tool_call') return []
    return [
      {
        id: step.id,
        name: step.tool_call?.function.name || 'tool',
        status:
          step.status === 'completed'
            ? step.tool_summary?.error
              ? 'error'
              : 'ok'
            : step.status === 'running'
              ? 'running'
              : 'error',
        ...(step.tool_summary ? { summary: step.tool_summary } : {}),
      },
    ]
  })
}
