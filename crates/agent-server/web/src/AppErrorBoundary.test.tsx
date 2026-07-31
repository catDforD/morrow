// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import AppErrorBoundary from './AppErrorBoundary'

let root: Root | null = null

describe('AppErrorBoundary', () => {
  beforeEach(() => {
    ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
      .IS_REACT_ACT_ENVIRONMENT = true
    vi.spyOn(console, 'error').mockImplementation(() => undefined)
  })

  afterEach(async () => {
    await act(async () => root?.unmount())
    root = null
    document.body.replaceChildren()
    vi.restoreAllMocks()
  })

  it('replaces a crashed application with an explicit reload screen', async () => {
    const container = document.createElement('div')
    document.body.append(container)
    root = createRoot(container)
    await act(async () => {
      root?.render(
        <AppErrorBoundary>
          <Crash />
        </AppErrorBoundary>,
      )
    })

    expect(document.querySelector('.app-error-boundary')).not.toBeNull()
    expect(document.body.textContent).toContain('Morrow could not continue')
    expect(document.body.textContent).toContain('render failed')
    expect(document.querySelector('button')?.textContent).toContain('Reload')
  })
})

function Crash(): never {
  throw new Error('render failed')
}
