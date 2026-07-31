// @vitest-environment jsdom

import { act } from 'react'
import type { ReactNode } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  InspectorDrawer,
  PersistentSubagentPanel,
  subagentTranscriptMessages,
} from './App'
import type { SessionProjection, SubagentTranscriptSnapshot } from './types'

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

  it('shows concise agent details without rendering the complete transcript', async () => {
    const transcript = buildTranscript()
    await render(
      <PersistentSubagentPanel
        instances={[transcript.instance]}
        transcript={transcript}
        onSend={() => {}}
        onInspect={() => {}}
        onCancel={() => {}}
        onDelete={() => {}}
      />,
    )

    expect(document.querySelector('.subagent-detail-card')).not.toBeNull()
    expect(document.body.textContent).toContain('Agent details')
    expect(document.body.textContent).toContain('Reviewer')
    expect(document.body.textContent).toContain('独立审查与审批后检查')
    expect(document.body.textContent).toContain('review the workspace')
    expect(document.body.textContent).toContain('Review completed with three findings.')
    expect(document.body.textContent).not.toContain('SHOULD_NOT_APPEAR')
    expect(document.body.textContent).not.toContain('first question')
    expect(document.body.textContent).not.toContain('first answer')
    expect(document.querySelector('.subagent-message-transcript')).toBeNull()
    expect(document.querySelector('.subagent-event-log')).toBeNull()
    expect(document.body.textContent).not.toContain('Show event log')
    expect(document.body.textContent).toContain('event log reached its 16 MiB limit')
    expect(document.querySelector<HTMLDetailsElement>('.subagent-followup-disclosure')?.open)
      .toBe(false)
  })

  it('does not expose manual creation controls and explains the empty state', async () => {
    await render(
      <PersistentSubagentPanel
        instances={[]}
        transcript={null}
        onSend={() => {}}
        onInspect={() => {}}
        onCancel={() => {}}
        onDelete={() => {}}
      />,
    )

    expect(document.querySelector('.subagent-spawn-form')).toBeNull()
    expect(document.querySelector('select')).toBeNull()
    expect(document.querySelector('.subagent-instance-section')?.textContent).toContain('Current agents')
    expect(document.querySelector('.subagent-empty-state')?.textContent).toContain('No agents yet')
    expect(document.querySelector('.subagent-empty-state')?.textContent).toContain('主 Agent')
  })

  it('wires inspect, continue and delete controls to existing instance actions', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true)
    const transcript = buildTranscript()
    const onSend = vi.fn()
    const onInspect = vi.fn()
    const onDelete = vi.fn()
    await render(
      <PersistentSubagentPanel
        instances={[transcript.instance]}
        transcript={transcript}
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

    const followupDisclosure = document.querySelector<HTMLDetailsElement>(
      '.subagent-followup-disclosure',
    )
    await act(async () => {
      followupDisclosure?.querySelector('summary')?.click()
    })
    expect(followupDisclosure?.open).toBe(true)

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

describe('InspectorDrawer', () => {
  it('only exposes the Run and Agents panels', async () => {
    await render(
      <InspectorDrawer
        open
        panel="run"
        selectedEntry={undefined}
        runningTurn={null}
        pendingApproval={null}
        approvalQueue={[]}
        subagents={[]}
        subagentTranscript={null}
        onClose={() => {}}
        onPanelChange={() => {}}
        onSendSubagent={() => {}}
        onInspectSubagent={() => {}}
        onCancelSubagent={() => {}}
        onDeleteSubagent={() => {}}
      />,
    )

    const tabs = [...document.querySelectorAll<HTMLButtonElement>('.drawer-tab')]
      .map((button) => button.textContent?.trim())
    expect(tabs).toEqual(['Run', 'Agents'])
    expect(document.body.textContent).not.toContain('Recent')
    expect(document.body.textContent).not.toContain('Tools')
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
  const result = `Review completed with three findings. ${'Supporting detail. '.repeat(40)}SHOULD_NOT_APPEAR`
  const session: SessionProjection = {
    session_id: 'session-subagent-1',
    revision: 4,
    turns: [{
      id: 'turn-1',
      operation_id: 'operation-1',
      index: 0,
      status: 'completed',
      user_message: { role: 'user', content: 'first question' },
      model: {
        provider_id: 'provider-1',
        provider_name: 'Provider',
        model_id: 'model-1',
        model_name: 'Model',
        reasoning: 'high',
      },
      permissions: { mode: 'read_only', shell: 'prompt' },
      messages: [
        { role: 'user', content: 'first question' },
        { role: 'assistant', content: 'first answer' },
      ],
      steps: [],
      notices: [],
      started_at_ms: 1,
      completed_at_ms: 2,
    }],
    context: {
      summary: 'summary',
      covered_through_turn_id: 'turn-1',
      messages: [{ role: 'system', content: 'compacted-only active thread' }],
    },
    diagnostics: [],
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
      summary: {
        instance_id: 'subagent-1',
        run_id: 'subrun-1',
        role: 'reviewer',
        status: 'completed',
        task: 'review the workspace',
        result,
        model_calls: 2,
        tool_calls: 3,
        file_changes: [{
          path: 'src/lib.rs',
          operation: 'update',
          replacements: 1,
          created: false,
          overwritten: false,
          deleted: false,
        }],
        shell_commands: [{
          command: 'cargo test',
          exit_code: 0,
          timed_out: false,
          stdout_truncated: false,
          stderr_truncated: false,
        }],
        started_at_ms: 1,
        completed_at_ms: 2,
        truncated: false,
      },
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
