// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, describe, expect, it } from 'vitest'
import RunInspector from './RunInspector'
import type { RunStep, TimelineItem } from './types'

let root: Root | null = null
afterEach(async () => {
  await act(async () => root?.unmount())
  root = null
  document.body.replaceChildren()
})

describe('execution inspector', () => {
  it('prioritizes failures and changed files, retains tool details and omits duplicate model thoughts', async () => {
    const steps: RunStep[] = [
      { id: 'model', kind: 'model', title: 'Model name', reasoning: 'Private reasoning text', detail: 'Provider high', status: 'ok' },
      { id: 'write', kind: 'tool', title: 'edit_file', status: 'ok', output: 'Updated', summary: { files: [{ path: 'src/App.tsx', operation: 'update', replacements: 1, created: false, overwritten: false, deleted: false }], diff: '-old\n+new' } },
      { id: 'shell', kind: 'tool', title: 'shell_command', status: 'error', summary: { error: 'Tests failed' } },
    ]
    const timeline: TimelineItem[] = [{ kind: 'run', id: 'run', trace: { id: 'run', status: 'failed', collapsed: false, startedAt: 'now', steps, toolCount: 2 } }]
    ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true
    const element = document.createElement('div')
    document.body.append(element)
    root = createRoot(element)
    await act(async () => root?.render(<RunInspector timeline={timeline} runningTurn={null} approvalQueue={[]} renderApproval={() => null} />))
    expect(document.querySelector('.inspector-status-line')?.textContent).toBe('执行未完成')
    expect(document.body.textContent).not.toContain('Private reasoning text')
    expect(document.body.textContent).not.toContain('Model name')
    expect(document.querySelector('[aria-label="失败项"]')?.textContent).toContain('Tests failed')
    expect(document.querySelector('[aria-label="修改的文件"]')?.textContent).toContain('src/App.tsx')
    expect(document.querySelector('[aria-label="文件差异"]')?.textContent).toBe('-old\n+new')
    expect(document.querySelector('[aria-label="工具输出"]')?.textContent).toBe('Updated')
    expect(document.querySelector<HTMLDetailsElement>('.inspector-more')?.open).toBe(false)
    expect(document.querySelectorAll('.inspector-metric')).toHaveLength(0)
  })
})
