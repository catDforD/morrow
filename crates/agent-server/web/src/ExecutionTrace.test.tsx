// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import ExecutionTrace, { executionSummary, groupExecutionSteps, toolStepLabel } from './ExecutionTrace'
import TimelineNotices, { groupTimelineNotices } from './TimelineNotices'
import type { RunStep, RunTrace, TimelineItem, TimelineNoticeItem } from './types'

let root: Root | null = null

function tool(id: string, name: string, args: unknown, status: RunStep['status'] = 'ok'): RunStep {
  return { id, kind: 'tool', status, title: name, toolCall: { id, type: 'function', function: { name, arguments: JSON.stringify(args) } } }
}

function trace(steps: RunStep[]): RunTrace {
  return { id: 'run', status: 'running', collapsed: false, startedAt: 'now', toolCount: steps.filter((step) => step.kind === 'tool').length, steps }
}

beforeEach(() => {
  ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true
  const container = document.createElement('div')
  document.body.append(container)
  root = createRoot(container)
})

afterEach(async () => {
  await act(async () => root?.unmount())
  root = null
  document.body.replaceChildren()
})

async function renderTrace(value: RunTrace) {
  await act(async () => root?.render(<ExecutionTrace trace={value} onToggle={vi.fn()} renderSpecialStep={() => null} />))
}

describe('continuous execution', () => {
  it('merges adjacent thoughts and successful reads without crossing tools, errors or approvals', () => {
    const steps: RunStep[] = [
      { id: 'model-1', kind: 'model', status: 'ok', title: 'Model', reasoning: 'First thought.' },
      { id: 'model-2', kind: 'model', status: 'ok', title: 'Model', reasoning: 'Next thought.' },
      tool('read-1', 'read_file', { path: 'App.tsx' }),
      tool('read-2', 'read_file', { path: 'styles.css' }),
      tool('read-error', 'read_file', { path: 'missing' }, 'error'),
      { ...tool('read-approval', 'read_file', { path: 'private' }, 'approval'), kind: 'approval' },
      { id: 'model-3', kind: 'model', status: 'running', title: 'Model', reasoning: 'Continue.' },
    ]
    expect(groupExecutionSteps(steps).map((group) => group.steps.map((step) => step.id))).toEqual([
      ['model-1', 'model-2'], ['read-1', 'read-2'], ['read-error'], ['read-approval'], ['model-3'],
    ])
    expect(steps).toHaveLength(7)
  })

  it('streams normal paragraphs and preserves an expanded tool while later thoughts arrive', async () => {
    const steps: RunStep[] = [
      tool('read-1', 'read_file', { path: 'App.tsx' }),
      { id: 'model', kind: 'model', status: 'running', title: 'DeepSeek', reasoning: '检查入口。' },
    ]
    await renderTrace(trace(steps))
    const disclosure = document.querySelector<HTMLDetailsElement>('.execution-action')!
    await act(async () => {
      disclosure.open = true
      disclosure.dispatchEvent(new Event('toggle'))
    })
    await renderTrace(trace([steps[0], { ...steps[1], reasoning: '检查入口。\n\n接着检查布局。' }]))
    expect(document.querySelector('.execution-action')).toBe(disclosure)
    expect(disclosure.open).toBe(true)
    expect(document.querySelectorAll('.execution-thought p')).toHaveLength(2)
    expect(document.querySelector('.execution-thought pre')).toBeNull()
    expect(document.body.textContent).not.toContain('DeepSeek')
    expect(document.body.textContent).not.toContain('思考过程')
  })

  it('keeps a running read expanded when it completes and merges with the preceding read', async () => {
    const first = tool('a', 'read_file', { path: 'App.tsx' })
    const second = tool('b', 'read_file', { path: 'styles.css' }, 'running')
    await renderTrace(trace([first, second]))
    const disclosure = document.querySelectorAll<HTMLDetailsElement>('.execution-action')[1]
    await act(async () => {
      disclosure.open = true
      disclosure.dispatchEvent(new Event('toggle'))
    })
    await renderTrace(trace([first, { ...second, status: 'ok' }]))
    expect(document.querySelectorAll('.execution-action')).toHaveLength(1)
    expect(document.querySelector<HTMLDetailsElement>('.execution-action')?.open).toBe(true)
  })

  it('keeps each grouped call’s arguments and output and shows failures and approvals', async () => {
    const steps = [
      { ...tool('a', 'read_file', { path: 'App.tsx' }), output: 'app contents' },
      { ...tool('b', 'read_file', { path: 'styles.css' }), output: 'style contents' },
      { ...tool('c', 'shell_command', { command: 'cargo test' }, 'error'), summary: { error: 'exit 1' } },
      { ...tool('d', 'shell_command', { command: 'cargo build' }, 'approval'), kind: 'approval' as const },
    ]
    await renderTrace(trace(steps))
    expect(document.querySelector('.execution-action > summary')?.textContent).toBe('读取 App.tsx、styles.css')
    expect([...document.querySelectorAll('[aria-label="工具输出"]')].map((el) => el.textContent)).toEqual(['app contents', 'style contents'])
    expect(document.querySelector('.execution-action.error > summary')?.textContent).toContain('失败')
    expect(document.querySelector('.execution-action.approval > summary')?.textContent).toContain('待确认')
    expect(document.querySelector('.execution-error')?.textContent).toBe('exit 1')
  })

  it('counts unique successful file operations and renders only a summary when collapsed', async () => {
    const value = trace([
      tool('a', 'read_file', { path: 'App.tsx' }), tool('b', 'read_file', { path: 'App.tsx' }),
      tool('c', 'read_file', { path: 'missing' }, 'error'),
      { ...tool('d', 'edit_file', { path: 'App.tsx' }), summary: { files: [{ path: 'App.tsx', operation: 'update', replacements: 1, created: false, overwritten: false, deleted: false }] } },
      tool('e', 'shell_command', { command: 'cargo test' }),
    ])
    expect(executionSummary(value)).toBe('读取 1 个文件 · 修改 1 个文件 · 运行 1 条命令 · 1 次调用失败')
    await renderTrace({ ...value, status: 'completed', collapsed: true })
    expect(document.querySelector('.execution-toggle')?.getAttribute('aria-expanded')).toBe('false')
    expect(document.querySelector('.execution-stream')).toBeNull()
  })

  it('handles malformed tool arguments without losing their original content', async () => {
    const step = tool('a', 'read_file', {})
    step.toolCall!.function.arguments = '{broken'
    expect(toolStepLabel(step)).toBe('读取文件')
    await renderTrace(trace([step]))
    expect(document.querySelector('[aria-label="调用参数"]')?.textContent).toBe('{broken')
  })
})

describe('execution notices', () => {
  const notices: TimelineNoticeItem[] = [
    { kind: 'notice', id: 'fetch', title: 'Notice', tone: 'neutral', detail: 'mcp server fetch: MCP request initialize failed: traceback' },
    { kind: 'notice', id: 'playwright', title: 'Notice', tone: 'neutral', detail: 'mcp server playwright: failed to send MCP HTTP request' },
  ]

  it('groups adjacent notices within their timeline position and preserves all error details', async () => {
    const user: TimelineItem = { kind: 'message', id: 'user', role: 'user', content: 'hello' }
    const groups = groupTimelineNotices([user, ...notices, user, { ...notices[0], id: 'other-turn' }])
    expect(groups.map((group) => group.kind)).toEqual(['message', 'notices', 'message', 'notices'])
    await act(async () => root?.render(<TimelineNotices notices={notices} />))
    const disclosure = document.querySelector('details')!
    expect(disclosure.open).toBe(false)
    expect(disclosure.querySelector('summary')?.textContent).toBe('2 项连接异常')
    expect([...document.querySelectorAll('pre')].map((el) => el.textContent)).toEqual(notices.map((notice) => notice.detail))
  })

  it('does not label unrelated notices as connection errors', async () => {
    await act(async () => root?.render(<TimelineNotices notices={[{ ...notices[0], detail: 'Session migrated' }]} />))
    expect(document.querySelector('summary')?.textContent).toBe('1 项提示')
  })
})
