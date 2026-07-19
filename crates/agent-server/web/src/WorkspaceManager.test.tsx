// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const bridge = vi.hoisted(() => ({
  connectWsl: vi.fn(),
  listWslDistributions: vi.fn(),
  listenWslLogs: vi.fn(),
  prepareWsl: vi.fn(),
  remoteRequest: vi.fn(),
}))

vi.mock('./desktop', () => bridge)

import {
  ProjectsDialog,
  RemoteConnectionDialog,
  WorkspaceMenu,
} from './WorkspaceManager'

let roots: Root[] = []

describe('workspace manager', () => {
  beforeEach(() => {
    Object.values(bridge).forEach((mock) => mock.mockReset())
    bridge.listenWslLogs.mockResolvedValue(() => undefined)
  })

  afterEach(async () => {
    await act(async () => {
      roots.forEach((root) => root.unmount())
    })
    roots = []
    document.body.replaceChildren()
  })

  it('opens local, remote, project, and reconnect actions from the project picker', async () => {
    const onOpenLocal = vi.fn()
    const onOpenRemote = vi.fn()
    const onOpenProjects = vi.fn()
    const onReconnect = vi.fn()
    await render(
      <WorkspaceMenu
        name="default"
        path="/home/user/.morrow/workspaces/default"
        recentWorkspaces={[
          {
            index: 3,
            label: 'remote-project',
            target: 'Ubuntu · WSL',
            path: '/home/user/code/remote-project',
          },
        ]}
        disabled={false}
        onOpenLocal={onOpenLocal}
        onOpenRemote={onOpenRemote}
        onOpenProjects={onOpenProjects}
        onReconnect={onReconnect}
      />,
    )

    await click(buttonByText('Default project'))
    await click(buttonByText('remote-projectUbuntu · WSL'))
    expect(onReconnect).toHaveBeenCalledWith(3)

    await click(buttonByText('Default project'))
    await click(buttonByText('Open local folder'))
    expect(onOpenLocal).toHaveBeenCalledOnce()

    await click(buttonByText('Default project'))
    await click(buttonByText('Remote connection'))
    expect(onOpenRemote).toHaveBeenCalledOnce()

    await click(buttonByText('Default project'))
    await click(buttonByText('Manage projects'))
    expect(onOpenProjects).toHaveBeenCalledOnce()
  })

  it('shows recent projects and exposes WSL reconnect from Projects', async () => {
    const reconnect = vi.fn()
    await render(
      <ProjectsDialog
        open
        state={{
          isMaximized: false,
          activeWorkspace: {
            kind: 'local',
            path: '/home/user/.morrow/workspaces/default',
          },
          recentWorkspaces: [
            {
              index: 0,
              label: 'default',
              target: 'Local',
              path: '/home/user/.morrow/workspaces/default',
            },
            {
              index: 1,
              label: 'morrow',
              target: 'Ubuntu · WSL',
              path: '/home/user/code/morrow',
            },
          ],
        }}
        busyIndex={null}
        onClose={() => undefined}
        onOpenLocal={() => undefined}
        onOpenRemote={() => undefined}
        onReconnect={reconnect}
      />,
    )

    expect(document.body.textContent).toContain('Default project')
    expect(document.body.textContent).toContain('Ubuntu · WSL')
    await click(buttonByText('morrow/home/user/code/morrowUbuntu · WSL'))
    expect(reconnect).toHaveBeenCalledWith(1)
  })

  it('offers WSL in the remote dialog and loads supported distributions', async () => {
    bridge.listWslDistributions.mockResolvedValue([
      { name: 'Ubuntu', version: 2, is_default: true },
    ])
    await render(
      <RemoteConnectionDialog open platform="windows" onClose={() => undefined} />,
    )

    await click(buttonByText('WSLWindows Subsystem for Linux'))
    expect(bridge.listWslDistributions).toHaveBeenCalledOnce()
    expect(document.body.textContent).toContain('Configure WSL')
    expect(document.body.textContent).toContain('Ubuntu · Default · WSL 2')
  })
})

async function render(element: React.ReactNode): Promise<void> {
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  roots.push(root)
  await act(async () => {
    root.render(element)
  })
}

async function click(element: Element): Promise<void> {
  await act(async () => {
    element.dispatchEvent(new MouseEvent('click', { bubbles: true }))
  })
  await act(async () => undefined)
}

function buttonByText(text: string): HTMLButtonElement {
  const button = Array.from(document.querySelectorAll('button')).find(
    (candidate) => candidate.textContent?.replace(/\s+/g, '').trim() === text.replace(/\s+/g, ''),
  )
  if (!button) throw new Error(`Could not find button: ${text}`)
  return button
}
