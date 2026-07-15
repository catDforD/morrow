// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const desktop = vi.hoisted(() => ({
  getDesktopPlatform: vi.fn(),
  getDesktopShellState: vi.fn(),
}))

vi.mock('./desktop', () => desktop)
vi.mock('./App', () => ({ default: () => <div>Morrow app</div> }))
vi.mock('./DesktopShell', () => ({
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}))

import DesktopBootstrap from './DesktopBootstrap'

let root: Root | null = null

describe('desktop bootstrap', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    desktop.getDesktopPlatform.mockReset()
    desktop.getDesktopShellState.mockReset()
  })

  afterEach(async () => {
    await act(async () => root?.unmount())
    root = null
    document.body.replaceChildren()
    vi.useRealTimers()
  })

  it('opens the web app directly outside the desktop shell', async () => {
    desktop.getDesktopPlatform.mockReturnValue(null)

    await renderBootstrap()

    expect(document.body.textContent).toContain('Morrow app')
    expect(desktop.getDesktopShellState).not.toHaveBeenCalled()
  })

  it('waits for the default workspace before opening the desktop app', async () => {
    desktop.getDesktopPlatform.mockReturnValue('windows')
    desktop.getDesktopShellState
      .mockResolvedValueOnce({
        isMaximized: false,
        recentWorkspaces: [],
        activeWorkspace: null,
      })
      .mockResolvedValueOnce({
        isMaximized: false,
        recentWorkspaces: [],
        activeWorkspace: {
          kind: 'local',
          path: 'C:\\Users\\morrow\\.morrow\\workspaces\\default',
        },
      })

    await renderBootstrap()
    expect(document.body.textContent).toContain('Opening your Morrow workspace')
    expect(document.body.textContent).toContain('A default workspace will be used')

    await act(async () => {
      await vi.advanceTimersByTimeAsync(200)
    })

    expect(document.body.textContent).toContain('Morrow app')
  })
})

async function renderBootstrap(): Promise<void> {
  const container = document.createElement('div')
  document.body.append(container)
  root = createRoot(container)
  await act(async () => {
    root?.render(<DesktopBootstrap />)
  })
}
