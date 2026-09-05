// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import MarkdownContent from './MarkdownContent'

let root: Root | null = null
const writeText = vi.fn()

beforeEach(() => {
  ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
    .IS_REACT_ACT_ENVIRONMENT = true
  Object.defineProperty(navigator, 'clipboard', {
    configurable: true,
    value: { writeText },
  })
  writeText.mockReset().mockResolvedValue(undefined)
})

afterEach(async () => {
  await act(async () => root?.unmount())
  root = null
  document.body.replaceChildren()
})

describe('MarkdownContent code blocks', () => {
  it('copies the complete code without the language or button text', async () => {
    await render('```rust\nfn main() {\n    println!("hello");\n}\n```')
    await act(async () => document.querySelector<HTMLButtonElement>('button')?.click())
    expect(writeText).toHaveBeenCalledWith('fn main() {\n    println!("hello");\n}\n')
    expect(document.body.textContent).toContain('已复制')
    expect(document.querySelector('.markdown-code-header > span')?.textContent).toBe('rust')
  })

  it('keeps code selectable and reports a clipboard failure', async () => {
    writeText.mockRejectedValue(new Error('clipboard denied'))
    await render('```\ncargo test\n```')
    await act(async () => document.querySelector<HTMLButtonElement>('button')?.click())
    expect(document.body.textContent).toContain('复制失败，请手动选择')
    expect(document.querySelector('pre')?.textContent).toBe('cargo test\n')
  })
})

async function render(content: string) {
  const container = document.createElement('div')
  document.body.append(container)
  root = createRoot(container)
  await act(async () => root?.render(<MarkdownContent content={content} />))
}
