// @vitest-environment jsdom

import { invoke } from '@tauri-apps/api/core'
import { act, useState } from 'react'
import type { ReactNode } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import DesktopShell from './DesktopShell'
import type { DesktopPlatform } from './desktop'

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))

const invokeMock = vi.mocked(invoke)
let roots: Root[] = []

describe('DesktopShell', () => {
  beforeEach(() => {
    ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
      .IS_REACT_ACT_ENVIRONMENT = true
    invokeMock.mockReset()
    invokeMock.mockImplementation(async (command) => {
      if (command === 'desktop_shell_state') {
        return {
          isMaximized: false,
          recentWorkspaces: [
            { index: 3, label: 'C:\\code\\morrow' },
            { index: 7, label: '/work/another-project' },
          ],
        }
      }
      return undefined
    })
    vi.stubGlobal(
      'requestAnimationFrame',
      (callback: FrameRequestCallback) => window.setTimeout(callback, 0),
    )
    vi.stubGlobal('cancelAnimationFrame', (id: number) => clearTimeout(id))
    Object.defineProperty(document, 'execCommand', {
      configurable: true,
      value: vi.fn().mockReturnValue(false),
    })
  })

  afterEach(async () => {
    await act(async () => {
      roots.forEach((root) => root.unmount())
    })
    roots = []
    document.body.replaceChildren()
    Reflect.deleteProperty(window, '__MORROW_DESKTOP__')
    vi.unstubAllGlobals()
  })

  it('leaves the browser dashboard unchanged and never invokes desktop IPC', async () => {
    await renderShell(<main data-testid="dashboard">Dashboard</main>)

    expect(document.querySelector('[data-testid="dashboard"]')).not.toBeNull()
    expect(document.querySelector('.desktop-shell')).toBeNull()
    expect(invokeMock).not.toHaveBeenCalled()
  })

  it('opens menus with access keys and supports roving arrow-key focus', async () => {
    setDesktopMarker('windows')
    await renderShell(<main>Workspace</main>)

    await pressKey(window, 'e', { altKey: true, ctrlKey: true })
    await pressKey(window, 'F10', { shiftKey: true })
    expect(document.querySelector('[role="menu"]')).toBeNull()

    await pressKey(window, 'e', { altKey: true })
    expect(menuByName('Edit')).not.toBeNull()
    expect(document.activeElement?.textContent).toContain('Undo')

    await pressKey(document.activeElement!, 'ArrowDown')
    expect(document.activeElement?.textContent).toContain('Redo')

    await pressKey(document.activeElement!, 'ArrowRight')
    expect(menuByName('Window')).not.toBeNull()
    expect(menuByName('Edit')).toBeNull()

    await pressKey(document.activeElement!, 'Escape')
    expect(document.querySelector('[role="menu"]')).toBeNull()
  })

  it('opens the recent submenu and sends only its opaque index to Rust', async () => {
    setDesktopMarker('windows')
    await renderShell(<main>Workspace</main>)

    await pressKey(window, 'f', { altKey: true })
    await pressKey(document.activeElement!, 'ArrowDown')
    expect(document.activeElement?.textContent).toContain('Open Recent')
    await pressKey(document.activeElement!, 'Enter')
    expect(buttonByText('morrow')).not.toBeNull()
    expect(document.activeElement?.textContent).toContain('morrow')
    expect(document.body.textContent).not.toContain('C:\\code\\morrow')

    await click(buttonByText('morrow'))
    expect(invokeMock).toHaveBeenCalledWith('desktop_action', {
      action: { type: 'open_recent', index: 3 },
    })
  })

  it('routes About Morrow into the existing settings callback', async () => {
    setDesktopMarker('windows')
    const onOpenAbout = vi.fn()
    await renderShell(<main>Workspace</main>, onOpenAbout)

    await pressKey(window, 'h', { altKey: true })
    await click(buttonByText('About Morrow'))

    expect(onOpenAbout).toHaveBeenCalledOnce()
    expect(document.querySelector('[role="menu"]')).toBeNull()
  })

  it('uses the integrated overlay bar on macOS without duplicating native menus', async () => {
    setDesktopMarker('macos')
    await renderShell(<main>Workspace</main>)

    expect(document.querySelector('.desktop-shell.desktop-macos')).not.toBeNull()
    expect(document.querySelector('.desktop-menu-bar')).toBeNull()
    expect(document.querySelector('.window-controls')).toBeNull()
    expect(invokeMock).toHaveBeenCalledWith('desktop_shell_state')
  })

  it('uses the unified full-width titlebar on Linux and keeps the sidebar brand below it', async () => {
    setDesktopMarker('linux')
    await renderShell(
      <div className="app-frame">
        <aside className="app-sidebar">
          <div className="sidebar-brand">Morrow</div>
        </aside>
        <main>Workspace</main>
      </div>,
    )

    const shell = document.querySelector<HTMLElement>(
      '.desktop-shell.desktop-linux',
    )!
    const titlebar = shell.querySelector<HTMLElement>('.desktop-titlebar')!
    const sidebar = shell.querySelector<HTMLElement>('.app-sidebar')!

    expect(titlebar.querySelector('.desktop-menu-bar')).not.toBeNull()
    expect(titlebar.querySelector('.desktop-titlebar-drag-region')).not.toBeNull()
    expect(titlebar.querySelector('.window-controls')).not.toBeNull()
    expect(titlebar.querySelector('.desktop-titlebar-brand')).toBeNull()
    expect(titlebar.querySelector('.desktop-workspace-title')).toBeNull()
    expect(sidebar.querySelector('.sidebar-brand')).not.toBeNull()
    expect(shell.style.getPropertyValue('--desktop-titlebar-height')).toBe(
      '40px',
    )
    expect(shell.children[0]).toBe(titlebar)
    expect(shell.children[1]?.classList.contains('desktop-shell-content')).toBe(
      true,
    )
  })

  it('preserves a controlled textarea selection while pasting from the Edit menu', async () => {
    setDesktopMarker('windows')
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: {
        readText: vi.fn().mockResolvedValue('desktop'),
        writeText: vi.fn().mockResolvedValue(undefined),
      },
    })
    await renderShell(<ControlledTextarea />)
    const textarea = document.querySelector('textarea')!
    textarea.focus()
    textarea.setSelectionRange(7, 10)

    await pressKey(window, 'e', { altKey: true })
    await click(buttonByText('Paste'))
    await act(async () => undefined)

    expect(textarea.value).toBe('Morrow desktop app')
    expect(document.activeElement).toBe(textarea)
  })
})

function ControlledTextarea() {
  const [value, setValue] = useState('Morrow web app')
  return <textarea value={value} onChange={(event) => setValue(event.target.value)} />
}

async function renderShell(
  children: ReactNode,
  onOpenAbout: () => void = () => undefined,
): Promise<void> {
  const container = document.createElement('div')
  document.body.append(container)
  const root = createRoot(container)
  roots.push(root)
  await act(async () => {
    root.render(
      <DesktopShell onOpenAbout={onOpenAbout}>
        {children}
      </DesktopShell>,
    )
  })
  await act(async () => undefined)
}

async function click(element: Element): Promise<void> {
  await act(async () => {
    element.dispatchEvent(new MouseEvent('click', { bubbles: true }))
  })
}

async function pressKey(
  target: EventTarget,
  key: string,
  options: KeyboardEventInit = {},
): Promise<void> {
  await act(async () => {
    target.dispatchEvent(
      new KeyboardEvent('keydown', { bubbles: true, key, ...options }),
    )
  })
  await act(async () => new Promise((resolve) => setTimeout(resolve, 1)))
}

function menuByName(name: string): Element | null {
  const trigger = buttonByText(name)
  const id = trigger?.getAttribute('aria-controls')
  if (id) return document.getElementById(id)
  return trigger?.parentElement?.querySelector('[role="menu"]') ?? null
}

function buttonByText(text: string): HTMLButtonElement {
  const button = Array.from(document.querySelectorAll('button')).find(
    (candidate) =>
      candidate.textContent?.trim() === text ||
      candidate.querySelector(':scope > span')?.textContent?.trim() === text,
  )
  if (!button) throw new Error(`Could not find button: ${text}`)
  return button
}

function setDesktopMarker(platform: DesktopPlatform): void {
  Object.defineProperty(window, '__MORROW_DESKTOP__', {
    configurable: true,
    value: Object.freeze({ platform }),
  })
}
