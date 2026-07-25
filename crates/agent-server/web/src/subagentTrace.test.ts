import { describe, expect, it } from 'vitest'
import {
  persistentSubagentHistory,
  persistentSubagentSnapshotStep,
  finishedSubagentStep,
  runningSubagentStep,
  startingPersistentSubagentStep,
  subagentHistory,
} from './subagentTrace'
import type { Message } from './types'

describe('subagentHistory', () => {
  it('reconstructs a completed delegated task', () => {
    const messages: Message[] = [
      {
        role: 'assistant',
        tool_calls: [
          {
            id: 'call-1',
            type: 'function',
            function: {
              name: 'delegate_task',
              arguments: JSON.stringify({ task: 'Inspect session storage' }),
            },
          },
        ],
      },
      {
        role: 'tool',
        tool_call_id: 'call-1',
        content: JSON.stringify({
          ok: true,
          agent_id: 'builtin-01',
          agent_name: '后藤一里',
          task: 'Inspect session storage',
          result: 'Sessions are stored by workspace hash.',
          model_calls: 2,
          tool_calls: 3,
          truncated: false,
        }),
      },
    ]

    expect(subagentHistory(messages).get('call-1')).toEqual({
      task: 'Inspect session storage',
      agentId: 'builtin-01',
      agentName: '后藤一里',
      summary: {
        agent_id: 'builtin-01',
        agent_name: '后藤一里',
        task: 'Inspect session storage',
        result: 'Sessions are stored by workspace hash.',
        error: undefined,
        model_calls: 2,
        tool_calls: 3,
        truncated: false,
      },
    })
  })

  it('keeps the task visible when the result is not parseable', () => {
    const messages: Message[] = [
      {
        role: 'assistant',
        tool_calls: [
          {
            id: 'call-2',
            type: 'function',
            function: {
              name: 'delegate_task',
              arguments: JSON.stringify({ task: 'Find model selection flow' }),
            },
          },
        ],
      },
      { role: 'tool', tool_call_id: 'call-2', content: 'invalid json' },
    ]

    expect(subagentHistory(messages).get('call-2')).toEqual({
      task: 'Find model selection flow',
    })
  })

  it('keeps legacy completed results readable without inventing a name', () => {
    const messages: Message[] = [
      {
        role: 'assistant',
        tool_calls: [
          {
            id: 'legacy-call',
            type: 'function',
            function: {
              name: 'delegate_task',
              arguments: JSON.stringify({ task: 'Inspect legacy state' }),
            },
          },
        ],
      },
      {
        role: 'tool',
        tool_call_id: 'legacy-call',
        content: JSON.stringify({
          ok: true,
          task: 'Inspect legacy state',
          result: 'Legacy result',
          model_calls: 1,
          tool_calls: 0,
          truncated: false,
        }),
      },
    ]

    const entry = subagentHistory(messages).get('legacy-call')
    expect(entry?.agentName).toBeUndefined()
    expect(entry?.summary?.agent_name).toBeUndefined()
    expect(entry?.summary?.result).toBe('Legacy result')
  })

  it('builds live running and completed subagent steps', () => {
    expect(runningSubagentStep('call-3', 'builtin-01', '后藤一里', 'Inspect events')).toEqual({
      id: 'call-3',
      kind: 'subagent',
      status: 'running',
      title: '子智能体 · 后藤一里',
      detail: 'Inspect events',
      agentId: 'builtin-01',
      agentName: '后藤一里',
    })
    expect(
      finishedSubagentStep('call-3', true, {
        agent_id: 'builtin-01',
        agent_name: '后藤一里',
        task: 'Inspect events',
        result: 'Events use schema version 3.',
        model_calls: 1,
        tool_calls: 2,
        truncated: false,
      }),
    ).toEqual({
      id: 'call-3',
      kind: 'subagent',
      status: 'ok',
      title: '子智能体 · 后藤一里',
      detail: 'Inspect events',
      agentId: 'builtin-01',
      agentName: '后藤一里',
      summary: {
        subagent: {
          agent_id: 'builtin-01',
          agent_name: '后藤一里',
          task: 'Inspect events',
          result: 'Events use schema version 3.',
          model_calls: 1,
          tool_calls: 2,
          truncated: false,
        },
      },
    })
  })

  it('falls back to a generic title for legacy results without a name', () => {
    expect(
      finishedSubagentStep('legacy-call', true, {
        task: 'Inspect legacy state',
        result: 'Legacy result',
        model_calls: 1,
        tool_calls: 0,
        truncated: false,
      }).title,
    ).toBe('子智能体')
  })

  it('reconstructs a persistent spawn with its identity, role and status', () => {
    const messages: Message[] = [
      {
        role: 'assistant',
        tool_calls: [{
          id: 'spawn-1',
          type: 'function',
          function: {
            name: 'spawn_subagent',
            arguments: JSON.stringify({
              role: 'explore',
              task: 'Locate the session coordinator',
            }),
          },
        }],
      },
      {
        role: 'tool',
        tool_call_id: 'spawn-1',
        content: JSON.stringify({
          instance: {
            id: 'subagent-1',
            role: 'explore',
            identity: { id: 'builtin-01', name: '后藤一里' },
            status: 'running',
            created_at_ms: 1,
            updated_at_ms: 2,
            latest_run_id: 'subrun-1',
            latest_task: 'Locate the session coordinator',
            queue_reason: null,
            event_log_truncated: false,
          },
        }),
      },
    ]

    const entry = persistentSubagentHistory(messages).get('spawn-1')
    expect(entry?.role).toBe('explore')
    expect(entry?.task).toBe('Locate the session coordinator')
    expect(entry?.snapshot?.identity.name).toBe('后藤一里')
    expect(entry?.snapshot?.status).toBe('running')
    expect(
      persistentSubagentSnapshotStep('spawn-1', entry!.snapshot!),
    ).toMatchObject({
      kind: 'persistent_subagent',
      title: '子 Agent · 后藤一里',
      detail: 'Locate the session coordinator',
      agentId: 'builtin-01',
      agentName: '后藤一里',
      agentRole: 'explore',
      agentStatus: 'running',
      instanceId: 'subagent-1',
    })
  })

  it('uses a child-Agent placeholder instead of exposing the lifecycle tool name', () => {
    const step = startingPersistentSubagentStep('spawn-2')
    expect(step.kind).toBe('persistent_subagent')
    expect(step.title).toBe('正在启动子 Agent')
    expect(JSON.stringify(step)).not.toContain('spawn_subagent')
  })
})
