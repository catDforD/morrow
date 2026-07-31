import { describe, expect, it } from 'vitest'
import {
  emptySessionTimelineState,
  reduceSessionFrame,
  timelineFromSnapshot,
  toolsFromSnapshot,
} from './sessionTimelineReducer'
import type {
  SessionSnapshot,
  SessionStreamFrame,
  SessionUpdate,
  TurnProjection,
} from './types'

function turn(status: TurnProjection['status'] = 'running'): TurnProjection {
  return {
    id: 'turn-1',
    operation_id: 'operation-1',
    index: 0,
    status,
    user_message: { role: 'user', content: 'hello' },
    model: {
      provider_id: 'test',
      provider_name: 'Test',
      model_id: 'model',
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

function snapshot(): SessionSnapshot {
  return {
    schema_version: 2,
    session_name: 'default',
    session_id: 'session-1',
    revision: 1,
    cursor: { stream_id: 'stream-1', sequence: 4 },
    session: {
      session_id: 'session-1',
      revision: 1,
      turns: [turn()],
      context: { messages: [] },
      diagnostics: [],
    },
    active_operation: {
      operation_id: 'operation-1',
      turn_id: 'turn-1',
      phase: 'model_call',
      streaming: {
        model_call_id: 'model-call-1',
        content: 'par',
        reasoning: 'thinking',
      },
      cancellable: true,
    },
    permissions: { mode: 'workspace_write', shell: 'prompt' },
    approvals: [],
    subagents: [],
  }
}

function event(sequence: number, update: SessionUpdate): SessionStreamFrame {
  return {
    type: 'event',
    data: {
      schema_version: 2,
      stream_id: 'stream-1',
      sequence,
      session_revision: 2,
      timestamp_ms: sequence,
      update,
    },
  }
}

describe('session timeline reducer', () => {
  it('replaces state from a snapshot and applies only the next event', () => {
    const initial = reduceSessionFrame(emptySessionTimelineState(), {
      type: 'snapshot',
      data: snapshot(),
    })
    const completed = { ...turn('completed'), completed_at_ms: 10 }
    const next = reduceSessionFrame(
      initial,
      event(5, { type: 'turn_upserted', data: completed }),
    )

    expect(next.snapshot?.cursor.sequence).toBe(5)
    expect(next.snapshot?.session.turns[0].status).toBe('completed')
    expect(
      reduceSessionFrame(next, event(5, { type: 'turn_upserted', data: turn() })),
    ).toBe(next)
  })

  it('requires a fresh snapshot for gaps and stream epoch changes', () => {
    const initial = reduceSessionFrame(emptySessionTimelineState(), {
      type: 'snapshot',
      data: snapshot(),
    })
    expect(
      reduceSessionFrame(initial, event(6, { type: 'notice', data: { message: 'gap' } }))
        .resyncRequired,
    ).toBe(true)

    const wrongStream = event(5, { type: 'notice', data: { message: 'epoch' } })
    if (wrongStream.type === 'event') wrongStream.data.stream_id = 'stream-2'
    expect(reduceSessionFrame(initial, wrongStream).resyncRequired).toBe(true)
  })

  it('restores partial model output and tool state from canonical snapshot data', () => {
    const value = snapshot()
    value.session.turns[0].steps = [
      {
        id: 'tool-1',
        kind: 'tool_call',
        status: 'outcome_unknown',
        tool_call: {
          id: 'tool-1',
          type: 'function',
          function: { name: 'shell', arguments: '{}' },
        },
        error: 'interrupted',
      },
    ]
    value.approvals = [
      {
        id: 'approval-1',
        action: {
          kind: 'shell_command',
          command: 'pwd',
          cwd: '/workspace',
          timeout_secs: 30,
        },
        reason: 'test',
      },
    ]

    expect(timelineFromSnapshot(value)).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ kind: 'message', content: 'par' }),
      ]),
    )
    expect(toolsFromSnapshot(value)[0]).toMatchObject({
      id: 'tool-1',
      status: 'error',
    })
    expect(value.approvals[0].id).toBe('approval-1')
  })

  it('appends ordered model deltas to the active operation snapshot', () => {
    const initial = reduceSessionFrame(emptySessionTimelineState(), {
      type: 'snapshot',
      data: snapshot(),
    })
    const next = reduceSessionFrame(
      initial,
      event(5, {
        type: 'model_stream_delta',
        data: {
          operation_id: 'operation-1',
          model_call_id: 'model-call-1',
          text: 'tial',
          reasoning: ' more',
        },
      }),
    )
    expect(next.snapshot?.active_operation?.streaming).toMatchObject({
      content: 'partial',
      reasoning: 'thinking more',
    })
  })
})
