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
import { currentTask } from './SubagentInspector'

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

  it('shows the complete latest Markdown result and folds technical metadata', async () => {
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
    expect(document.body.textContent).toContain('Reviewer')
    expect(document.querySelector('.subagent-detail-header')?.textContent).toContain('审查')
    expect(document.querySelector('.subagent-detail-header')?.textContent).toContain('已完成')
    expect(document.body.textContent).toContain('review the workspace')
    expect(document.body.textContent).toContain('Review completed with three findings.')
    expect(document.querySelector('.subagent-result')?.textContent).toContain('RESULT_END')
    expect(document.querySelector('.subagent-result li strong')?.textContent).toBe('完整结果')
    expect(document.body.textContent).not.toContain('first question')
    expect(document.body.textContent).not.toContain('first answer')
    expect(document.querySelector('.subagent-message-transcript')).toBeNull()
    expect(document.querySelector('.subagent-event-log')).toBeNull()
    expect(document.body.textContent).not.toContain('Show event log')
    expect(document.querySelector('.subagent-technical-details')?.textContent).toContain('事件日志已截断')
    expect(document.querySelector<HTMLDetailsElement>('.subagent-technical-details')?.open)
      .toBe(false)
    expect(document.querySelector<HTMLDetailsElement>('.subagent-more')?.open).toBe(false)
  })

  it('does not expose manual creation controls and shows the empty state', async () => {
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
    expect(document.querySelector('.subagent-instance-section')?.textContent).toContain('子智能体')
    expect(document.querySelector('.subagent-empty-state')?.textContent).toContain('暂无子智能体')
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

    await setInput(
      document.querySelector<HTMLTextAreaElement>('.subagent-followup-form textarea'),
      'review the result',
    )
    await act(async () => {
      document.querySelector<HTMLFormElement>('.subagent-followup-form')?.requestSubmit()
    })
    expect(onSend).toHaveBeenCalledWith('subagent-1', 'review the result')

    await act(async () => {
      document.querySelector<HTMLDetailsElement>('.subagent-more')?.querySelector('summary')?.click()
      findButton('删除子智能体')?.click()
    })
    expect(window.confirm).toHaveBeenCalledOnce()
    expect(onDelete).toHaveBeenCalledWith('subagent-1')
  })

  it('copies the complete Markdown result', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } })
    const transcript = buildTranscript()
    await render(<PersistentSubagentPanel instances={[transcript.instance]} transcript={transcript} onSend={vi.fn()} onInspect={vi.fn()} onCancel={vi.fn()} onDelete={vi.fn()} />)
    await act(async () => document.querySelector<HTMLButtonElement>('[aria-label="复制结果"]')?.click())
    expect(writeText).toHaveBeenCalledWith(transcript.runs[0].summary?.result)
    expect(document.querySelector('.subagent-copy')?.textContent).toContain('已复制')
  })

  it('returns to the list and ignores another agent’s late transcript', async () => {
    const transcript = buildTranscript()
    const other = { ...transcript.instance, id: 'subagent-2', identity: { id: 'builtin-02', name: 'Explorer' } }
    const onInspect = vi.fn()
    await render(<PersistentSubagentPanel instances={[transcript.instance, other]} transcript={transcript} onSend={vi.fn()} onInspect={onInspect} onCancel={vi.fn()} onDelete={vi.fn()} />)
    await act(async () => document.querySelectorAll<HTMLButtonElement>('.subagent-instance-card')[1]?.click())
    expect(onInspect).toHaveBeenCalledWith('subagent-2')
    expect(document.querySelector('.subagent-detail-header')?.textContent).toContain('Explorer')
    expect(document.querySelector('.subagent-result')?.textContent).not.toContain('RESULT_END')
    expect(document.querySelector<HTMLButtonElement>('[aria-label="发送后续任务"]')?.disabled).toBe(true)
    await act(async () => document.querySelector<HTMLButtonElement>('.subagent-back')?.click())
    expect(document.querySelector('.subagent-detail-card')).toBeNull()
    expect(document.querySelector('.persistent-subagent-panel')?.classList.contains('has-selection')).toBe(false)
  })

  it('keeps active task cancellation available and disables destructive and followup actions', async () => {
    const transcript = buildTranscript()
    const active = { ...transcript.instance, status: 'running' as const, latest_run_id: 'new-run' }
    const onCancel = vi.fn()
    await render(<PersistentSubagentPanel instances={[active]} transcript={transcript} onSend={vi.fn()} onInspect={vi.fn()} onCancel={onCancel} onDelete={vi.fn()} />)
    expect(document.querySelector('.subagent-detail-header')?.textContent).toContain('执行中')
    expect(document.querySelector('.subagent-result')?.textContent).not.toContain('RESULT_END')
    expect(document.querySelector('.subagent-followup-form')).toBeNull()
    expect(findButton('删除子智能体')?.disabled).toBe(true)
    await act(async () => findButton('停止任务')?.click())
    expect(onCancel).toHaveBeenCalledWith('subagent-1')
  })

  it('uses fresh instance results and does not reuse a completed summary for a newer run', () => {
    const transcript = buildTranscript()
    const summary = transcript.runs[0].summary!
    const fresh = { ...transcript.instance, latest_run_id: 'new-run', latest_summary: { ...summary, run_id: 'new-run', result: 'new result' } }
    expect(currentTask(fresh, transcript).summary?.result).toBe('new result')
    expect(currentTask({ ...fresh, status: 'running' }, transcript).summary).toBeUndefined()
    expect(currentTask({ ...fresh, latest_summary: summary }, transcript).summary).toBeUndefined()
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
    expect(tabs).toEqual(['执行', '子智能体'])
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
  const result = `Review completed with three findings. ${'Supporting detail. '.repeat(40)}\n\n- **完整结果**\n\nRESULT_END`
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
    middleware_audit: [],
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
