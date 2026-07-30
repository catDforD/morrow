// @vitest-environment jsdom

import { describe, expect, it } from 'vitest'
import { historyRunTrace } from './legacySubagentTimeline'
import type { LegacyTurnRecord } from './legacySubagentTimeline'

describe('persistent subagent run trace', () => {
  it('replays spawn_subagent as an identified child Agent instead of a generic tool', () => {
    const record: LegacyTurnRecord = {
      turn: {
        status: 'completed',
        user_message: { role: 'user', content: 'Inspect the runtime' },
        assistant_message: { role: 'assistant', content: 'Started an explorer.' },
        steps: [
          { kind: 'model_call', status: 'completed', error: null },
          {
            kind: 'tool_call',
            status: 'completed',
            tool_name: 'spawn_subagent',
            tool_call_id: 'spawn-1',
            error: null,
          },
        ],
        error: null,
      },
      messages: [
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
              latest_task: 'Locate the session coordinator',
              event_log_truncated: false,
            },
          }),
        },
      ],
    }

    const trace = historyRunTrace(record, 0)
    expect(trace.steps[1]).toMatchObject({
      kind: 'persistent_subagent',
      title: '子 Agent · 后藤一里',
      agentName: '后藤一里',
      agentRole: 'explore',
      agentStatus: 'running',
    })
    expect(trace.steps[1]?.title).not.toBe('spawn_subagent')
    expect(trace.toolCount).toBe(0)
  })
})
