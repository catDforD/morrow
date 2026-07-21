import { describe, expect, it } from 'vitest'
import {
  finishedSubagentStep,
  runningSubagentStep,
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
      summary: {
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

  it('builds live running and completed subagent steps', () => {
    expect(runningSubagentStep('call-3', 'Inspect events')).toEqual({
      id: 'call-3',
      kind: 'subagent',
      status: 'running',
      title: 'Subagent',
      detail: 'Inspect events',
    })
    expect(
      finishedSubagentStep('call-3', true, {
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
      title: 'Subagent',
      detail: 'Inspect events',
      summary: {
        subagent: {
          task: 'Inspect events',
          result: 'Events use schema version 3.',
          model_calls: 1,
          tool_calls: 2,
          truncated: false,
        },
      },
    })
  })
})
