// @vitest-environment jsdom

import { act } from 'react'
import type { ReactNode } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { SubagentStepDisclosure, SubagentStepPanel } from './App'
import type { RunStep } from './types'

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
})

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
