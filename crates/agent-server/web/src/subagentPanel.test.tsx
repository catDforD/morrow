// @vitest-environment jsdom

import { act } from 'react'
import type { ReactNode } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import {
  PersistentSubagentStepCard,
  SubagentStepDisclosure,
  SubagentStepPanel,
} from './App'
import type { RunStep, SubagentInstanceSnapshot } from './types'

let roots: Root[] = []

describe('SubagentStepPanel', () => {
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
  })

  it('is collapsed by default and expands only when the user requests it', async () => {
    await render(
      <SubagentStepDisclosure
        step={{
          id: 'call-collapsed',
          kind: 'subagent',
          status: 'running',
          title: '子智能体 · 后藤一里',
          detail: 'Inspect the runtime',
        }}
      />,
    )
    const details = document.querySelector('details')
    const summary = document.querySelector('summary')

    expect(details?.open).toBe(false)
    expect(summary?.textContent).toContain('子智能体 · 后藤一里')

    await act(async () => {
      summary?.click()
    })

    expect(details?.open).toBe(true)

    await act(async () => {
      roots[0]?.render(
        <SubagentStepDisclosure
          step={{
            id: 'call-collapsed',
            kind: 'subagent',
            status: 'ok',
            title: '子智能体 · 后藤一里',
            detail: 'Inspect the runtime',
            summary: {
              subagent: {
                agent_name: '后藤一里',
                task: 'Inspect the runtime',
                result: 'Inspection complete.',
                model_calls: 1,
                tool_calls: 2,
                truncated: false,
              },
            },
          }}
        />,
      )
    })

    expect(details?.open).toBe(true)
    expect(details?.textContent).toContain('Inspection complete.')
  })

  it('shows the prompt and a waiting state while the subagent is running', async () => {
    await renderPanel({
      id: 'call-1',
      kind: 'subagent',
      status: 'running',
      title: '子智能体 · 后藤一里',
      detail: 'Inspect the runtime',
    })

    expect(document.body.textContent).toContain('提示词')
    expect(document.body.textContent).toContain('Inspect the runtime')
    expect(document.body.textContent).toContain('等待子智能体返回结果…')
  })

  it('renders the final report as Markdown with execution metadata', async () => {
    await renderPanel({
      id: 'call-2',
      kind: 'subagent',
      status: 'ok',
      title: '子智能体 · 山田凉',
      detail: 'Inspect events',
      summary: {
        subagent: {
          agent_name: '山田凉',
          task: 'Inspect events',
          result: '## Result\n\n- Event schema is stable.',
          model_calls: 2,
          tool_calls: 3,
          truncated: true,
        },
      },
    })

    expect(document.querySelector('.subagent-output h2')?.textContent).toBe(
      'Result',
    )
    expect(document.body.textContent).toContain('2 次模型调用 · 3 次只读工具')
    expect(document.body.textContent).toContain('结果已截断')
  })

  it('shows failures inside the output pane', async () => {
    await renderPanel({
      id: 'call-3',
      kind: 'subagent',
      status: 'error',
      title: '子智能体 · 喜多郁代',
      detail: 'Inspect failures',
      summary: {
        subagent: {
          agent_name: '喜多郁代',
          task: 'Inspect failures',
          error: 'subagent timed out after 300 seconds',
          model_calls: 1,
          tool_calls: 0,
          truncated: false,
        },
      },
    })

    expect(document.querySelector('.output-pane.failed')).not.toBeNull()
    expect(document.querySelector('.subagent-error')?.textContent).toContain(
      'timed out',
    )
  })

  it('renders a compact child Agent with identity, input and output', async () => {
    const step: RunStep = {
      id: 'spawn-1',
      kind: 'persistent_subagent',
      status: 'ok',
      title: '子 Agent · 后藤一里',
      detail: 'Locate the session coordinator',
      agentId: 'builtin-01',
      agentName: '后藤一里',
      agentRole: 'explore',
      agentStatus: 'running',
      instanceId: 'subagent-1',
    }
    const runningInstance = persistentInstance({
      status: 'running',
      latest_task: 'Locate the session coordinator',
    })
    await render(
      <PersistentSubagentStepCard
        profiles={[{
          id: 'builtin-01',
          name: '后藤一里',
          avatar_data_url: 'data:image/png;base64,avatar',
        }]}
        instances={[runningInstance]}
        step={step}
      />,
    )

    const details = document.querySelector<HTMLDetailsElement>('.persistent-agent-card')
    const summary = document.querySelector<HTMLElement>('.persistent-agent-summary')
    expect(details?.open).toBe(false)
    expect(document.querySelector('.persistent-agent-handoff')).toBeNull()
    expect(summary?.textContent).not.toContain('委派给子 Agent')
    expect(document.querySelector<HTMLImageElement>('.persistent-agent-avatar img')?.src)
      .toBe('data:image/png;base64,avatar')
    expect(document.querySelector('.persistent-agent-name')?.textContent).toContain('后藤一里')
    expect(document.querySelector('.persistent-agent-name')?.textContent).toContain('Explore')
    expect(document.querySelector('.persistent-agent-name')?.textContent).toContain(
      '职责：只读检索与代码探索',
    )
    expect(document.querySelector('.persistent-agent-status')?.textContent).toBe('执行中')
    expect(document.querySelector('.prompt-pane')?.textContent).toContain(
      'Locate the session coordinator',
    )
    expect(document.querySelector('.output-pane')?.textContent).toContain(
      '子 Agent 正在执行任务…',
    )
    expect(document.body.textContent).not.toContain('spawn_subagent')

    await act(async () => {
      summary?.click()
    })
    expect(details?.open).toBe(true)

    await act(async () => {
      roots[0]?.render(
        <PersistentSubagentStepCard
          profiles={[{
            id: 'builtin-01',
            name: '后藤一里',
            avatar_data_url: 'data:image/png;base64,avatar',
          }]}
          instances={[persistentInstance({
            status: 'idle',
            latest_task: 'Locate the session coordinator',
            latest_summary: {
              instance_id: 'subagent-1',
              run_id: 'subrun-1',
              role: 'explore',
              status: 'completed',
              task: 'Locate the session coordinator',
              result: '## Result\n\n- Coordinator located.',
              model_calls: 2,
              tool_calls: 3,
              file_changes: [],
              shell_commands: [],
              started_at_ms: 1,
              completed_at_ms: 2,
              truncated: false,
            },
          })]}
          step={step}
        />,
      )
    })

    expect(details?.open).toBe(true)
    expect(document.querySelector('.persistent-agent-status')?.textContent).toBe('已完成')
    expect(document.querySelector('.output-pane h2')?.textContent).toBe('Result')
    expect(document.querySelector('.output-pane')?.textContent).toContain(
      '2 次模型调用 · 3 次工具调用',
    )
  })

  it('does not attach a later follow-up result to an older spawn row', async () => {
    await render(
      <PersistentSubagentStepCard
        instances={[persistentInstance({
          status: 'idle',
          latest_task: 'Review the coordinator',
          latest_summary: {
            instance_id: 'subagent-1',
            run_id: 'subrun-2',
            role: 'explore',
            status: 'completed',
            task: 'Review the coordinator',
            result: 'This belongs to the follow-up run.',
            model_calls: 1,
            tool_calls: 1,
            file_changes: [],
            shell_commands: [],
            started_at_ms: 3,
            completed_at_ms: 4,
            truncated: false,
          },
        })]}
        step={{
          id: 'spawn-old',
          kind: 'persistent_subagent',
          status: 'ok',
          title: '子 Agent · 后藤一里',
          detail: 'Locate the session coordinator',
          agentName: '后藤一里',
          agentRole: 'explore',
          instanceId: 'subagent-1',
        }}
      />,
    )

    expect(document.querySelector('.output-pane')?.textContent).not.toContain(
      'This belongs to the follow-up run.',
    )
    expect(document.querySelector('.output-pane')?.textContent).toContain(
      '可在 AGENTS 中查看完整记录',
    )
  })

  it('shows a persistent run failure in the output pane', async () => {
    await render(
      <PersistentSubagentStepCard
        instances={[persistentInstance({
          status: 'failed',
          latest_task: 'Inspect failures',
          latest_summary: {
            instance_id: 'subagent-1',
            run_id: 'subrun-failed',
            role: 'explore',
            status: 'failed',
            task: 'Inspect failures',
            error: 'subagent timed out after 300 seconds',
            model_calls: 1,
            tool_calls: 0,
            file_changes: [],
            shell_commands: [],
            started_at_ms: 1,
            completed_at_ms: 2,
            truncated: false,
          },
        })]}
        step={{
          id: 'spawn-failed',
          kind: 'persistent_subagent',
          status: 'error',
          title: '子 Agent · 后藤一里',
          detail: 'Inspect failures',
          agentName: '后藤一里',
          agentRole: 'explore',
          instanceId: 'subagent-1',
        }}
      />,
    )

    expect(document.querySelector('.output-pane.failed')).not.toBeNull()
    expect(document.querySelector('.subagent-error')?.textContent).toContain('timed out')
  })
})

function persistentInstance(
  overrides: Partial<SubagentInstanceSnapshot> = {},
): SubagentInstanceSnapshot {
  return {
    id: 'subagent-1',
    role: 'explore',
    identity: { id: 'builtin-01', name: '后藤一里' },
    status: 'running',
    created_at_ms: 1,
    updated_at_ms: 2,
    latest_run_id: 'subrun-1',
    latest_task: 'Locate the session coordinator',
    event_log_truncated: false,
    ...overrides,
  }
}

async function renderPanel(step: RunStep): Promise<void> {
  await render(<SubagentStepPanel step={step} />)
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
