// @vitest-environment jsdom

import { act } from 'react'
import type { ReactNode } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  PersistentSubagentPanel,
  subagentTranscriptMessages,
} from './App'
import type { Session, SubagentTranscriptSnapshot } from './types'

let roots: Root[] = []

describe('PersistentSubagentPanel', () => {
  beforeEach(() => {
    ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
      .IS_REACT_ACT_ENVIRONMENT = true
  })

  afterEach(async () => {
    await act(async () => {
      roots.forEach((root) => root.unmount())
    })
    roots = []
    document.body.replaceChildren()
    vi.restoreAllMocks()
  })

  it('shows complete turn history and reveals the persisted event log on demand', async () => {
    const transcript = buildTranscript()
    await render(
      <PersistentSubagentPanel
        instances={[transcript.instance]}
        transcript={transcript}
        onSpawn={() => {}}
        onSend={() => {}}
        onInspect={() => {}}
        onCancel={() => {}}
        onDelete={() => {}}
      />,
    )

    expect(document.body.textContent).toContain('first question')
    expect(document.body.textContent).toContain('first answer')
    expect(document.body.textContent).not.toContain('compacted-only active thread')
    expect(document.querySelector('.subagent-event-log')).toBeNull()

    await act(async () => {
      findButton('Show event log')?.click()
    })

    expect(document.querySelector('.subagent-event-log')?.textContent).toContain(
      'approval_requested',
    )
    expect(document.body.textContent).toContain('Streaming deltas were truncated')
  })

  it('wires create, inspect, continue and delete controls to instance actions', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true)
    const transcript = buildTranscript()
    const onSpawn = vi.fn()
    const onSend = vi.fn()
    const onInspect = vi.fn()
    const onDelete = vi.fn()
    await render(
      <PersistentSubagentPanel
        instances={[transcript.instance]}
        transcript={transcript}
        onSpawn={onSpawn}
        onSend={onSend}
        onInspect={onInspect}
        onCancel={() => {}}
        onDelete={onDelete}
      />,
    )

    await act(async () => {
      document.querySelector<HTMLButtonElement>('.subagent-instance-card')?.click()
    })
    expect(onInspect).toHaveBeenCalledWith('subagent-1')

    await setInput(
      document.querySelector<HTMLTextAreaElement>('.subagent-spawn-form textarea'),
      'implement the change',
    )
    await act(async () => {
      const select = document.querySelector<HTMLSelectElement>('.subagent-spawn-form select')
      if (select) {
        select.value = 'worker'
        select.dispatchEvent(new Event('change', { bubbles: true }))
      }
      document.querySelector<HTMLFormElement>('.subagent-spawn-form')?.requestSubmit()
    })
    expect(onSpawn).toHaveBeenCalledWith('worker', 'implement the change')

    await setInput(
      document.querySelector<HTMLTextAreaElement>('.subagent-followup-form textarea'),
      'review the result',
    )
    await act(async () => {
      document.querySelector<HTMLFormElement>('.subagent-followup-form')?.requestSubmit()
    })
    expect(onSend).toHaveBeenCalledWith('subagent-1', 'review the result')

    await act(async () => {
      findButton('Delete')?.click()
    })
    expect(window.confirm).toHaveBeenCalledOnce()
    expect(onDelete).toHaveBeenCalledWith('subagent-1')
  })
})

describe('subagentTranscriptMessages', () => {
  it('uses immutable turn records instead of a compacted active thread', () => {
    const session = buildTranscript().session
    expect(subagentTranscriptMessages(session).map((message) => message.content)).toEqual([
      'first question',
      'first answer',
    ])
  })
})

function buildTranscript(): SubagentTranscriptSnapshot {
  const session: Session = {
    active_thread: {
      messages: [{ role: 'system', content: 'compacted-only active thread' }],
    },
    turns: [{
      turn: {
        status: 'completed',
        user_message: { role: 'user', content: 'first question' },
        assistant_message: { role: 'assistant', content: 'first answer' },
        steps: [],
        error: null,
      },
      messages: [
        { role: 'user', content: 'first question' },
        { role: 'assistant', content: 'first answer' },
      ],
    }],
    context: { summarized_turns: 1, summary: 'summary' },
  }
  return {
    instance: {
      id: 'subagent-1',
      role: 'reviewer',
      identity: { id: 'builtin-01', name: 'Reviewer' },
      status: 'idle',
      created_at_ms: 1,
      updated_at_ms: 2,
      latest_run_id: 'subrun-1',
      latest_task: 'review the workspace',
      event_log_truncated: true,
    },
    model: {
      provider_id: 'provider-1',
      provider_name: 'Provider',
      model_id: 'model-1',
      model_name: 'Model',
      reasoning: 'high',
    },
    permission_ceiling: { mode: 'read_only', shell: 'prompt' },
    role_config: {
      model_selection: null,
      prompt_suffix: '',
      timeout_secs: 300,
      max_tool_rounds: 99,
    },
    session,
    runs: [{
      id: 'subrun-1',
      task: 'review the workspace',
      status: 'completed',
      turn_index: 0,
      started_at_ms: 1,
      completed_at_ms: 2,
    }],
    events: [{
      schema_version: 7,
      timestamp_ms: 1,
      session: 'default',
      workspace_root: '/workspace',
      origin: {
        kind: 'subagent_run',
        instance_id: 'subagent-1',
        run_id: 'subrun-1',
        role: 'reviewer',
        turn_index: 0,
      },
      turn_index: 0,
      event_index: 0,
      event: {
        type: 'approval_requested',
        data: {
          id: 'approval-1',
          reason: 'run command',
          action: {
            kind: 'shell_command',
            command: 'cargo test',
            cwd: '/workspace',
            timeout_secs: 30,
          },
          origin: { kind: 'unknown' },
        },
      },
    }],
  }
}

async function render(element: ReactNode): Promise<void> {
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  roots.push(root)
  await act(async () => {
    root.render(element)
  })
}

async function setInput(
  input: HTMLInputElement | HTMLTextAreaElement | null,
  value: string,
): Promise<void> {
  await act(async () => {
    if (!input) return
    const setter = Object.getOwnPropertyDescriptor(
      input instanceof HTMLTextAreaElement
        ? HTMLTextAreaElement.prototype
        : HTMLInputElement.prototype,
      'value',
    )?.set
    setter?.call(input, value)
    input.dispatchEvent(new Event('input', { bubbles: true }))
    input.dispatchEvent(new Event('change', { bubbles: true }))
  })
}

function findButton(label: string): HTMLButtonElement | undefined {
  return [...document.querySelectorAll<HTMLButtonElement>('button')]
    .find((button) => button.textContent?.includes(label))
}
