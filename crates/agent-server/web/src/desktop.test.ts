// @vitest-environment jsdom

import { Channel, invoke } from '@tauri-apps/api/core'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  captureEditingContext,
  executeEditCommand,
  getDesktopPlatform,
  getDesktopShellState,
  listenRemoteEvents,
  runDesktopAction,
} from './desktop'
import type { DesktopPlatform } from './desktop'

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
  Channel: class<T> {
    onmessage: (message: T) => void = () => undefined
  },
}))

const invokeMock = vi.mocked(invoke)

describe('desktop bridge', () => {
  beforeEach(() => {
    invokeMock.mockReset()
    clearDesktopMarker()
  })

  afterEach(() => {
    clearDesktopMarker()
  })

  it('only enables the desktop shell for a valid injected platform', () => {
    expect(getDesktopPlatform()).toBeNull()

    setDesktopMarker('windows')
    expect(getDesktopPlatform()).toBe('windows')

    setDesktopMarker('macos')
    expect(getDesktopPlatform()).toBe('macos')

    setDesktopMarker('linux')
    expect(getDesktopPlatform()).toBe('linux')
  })

  it('invokes only the two desktop commands with the tagged action shape', async () => {
    invokeMock.mockResolvedValueOnce({
      isMaximized: true,
      recentWorkspaces: [{ index: 2, label: 'morrow' }],
    })

    await expect(getDesktopShellState()).resolves.toEqual({
      isMaximized: true,
      recentWorkspaces: [{ index: 2, label: 'morrow' }],
    })
    await runDesktopAction({ type: 'open_recent', index: 2 })

    expect(invokeMock).toHaveBeenNthCalledWith(1, 'desktop_shell_state')
    expect(invokeMock).toHaveBeenNthCalledWith(2, 'desktop_action', {
      action: { type: 'open_recent', index: 2 },
    })
  })

  it('streams remote events through a Tauri channel and unsubscribes once', async () => {
    invokeMock.mockResolvedValueOnce(17).mockResolvedValueOnce(undefined)
    const listener = vi.fn()

    const stop = await listenRemoteEvents(listener)
    const args = invokeMock.mock.calls[0][1] as Record<string, unknown>
    const channel = args.onEvent as Channel<{ value: number }>
    channel.onmessage({ value: 3 })
    stop()
    stop()

    expect(listener).toHaveBeenCalledWith({ value: 3 })
    expect(invokeMock).toHaveBeenNthCalledWith(1, 'desktop_remote_subscribe', {
      onEvent: channel,
    })
    expect(invokeMock).toHaveBeenNthCalledWith(2, 'desktop_remote_unsubscribe', {
      subscriptionId: 17,
    })
    expect(invokeMock).toHaveBeenCalledTimes(2)
  })
})

describe('desktop edit commands', () => {
  beforeEach(() => {
    document.body.replaceChildren()
    Object.defineProperty(document, 'execCommand', {
      configurable: true,
      value: vi.fn().mockReturnValue(false),
    })
  })

  it('pastes into a text control with the native setter and a bubbling input event', async () => {
    const textarea = document.createElement('textarea')
    textarea.value = 'Morrow desktop'
    document.body.append(textarea)
    textarea.focus()
    textarea.setSelectionRange(7, 14)
    const input = vi.fn()
    textarea.addEventListener('input', input)

    const context = captureEditingContext()
    const changed = await executeEditCommand('paste', context, document, {
      readText: vi.fn().mockResolvedValue('app'),
      writeText: vi.fn(),
    })

    expect(changed).toBe(true)
    expect(textarea.value).toBe('Morrow app')
    expect(textarea.selectionStart).toBe(10)
    expect(document.execCommand).toHaveBeenCalledWith(
      'insertText',
      false,
      'app',
    )
    expect(input).toHaveBeenCalledOnce()
    expect((input.mock.calls[0][0] as InputEvent).inputType).toBe(
      'insertFromPaste',
    )
  })

  it('cuts the saved selection and restores focus after menu interaction', async () => {
    const input = document.createElement('input')
    input.value = 'agent desktop app'
    document.body.append(input)
    input.focus()
    input.setSelectionRange(6, 13)
    const context = captureEditingContext()
    const writeText = vi.fn().mockResolvedValue(undefined)

    document.body.focus()
    const changed = await executeEditCommand('cut', context, document, {
      readText: vi.fn(),
      writeText,
    })

    expect(changed).toBe(true)
    expect(document.activeElement).toBe(input)
    expect(document.execCommand).toHaveBeenCalledWith('cut')
    expect(writeText).toHaveBeenCalledWith('desktop')
    expect(input.value).toBe('agent  app')
  })

  it('does not mutate readonly controls or clear the clipboard for an empty selection', async () => {
    const input = document.createElement('input')
    input.value = 'unchanged'
    input.readOnly = true
    document.body.append(input)
    input.focus()
    input.setSelectionRange(3, 3)
    const context = captureEditingContext()
    const readText = vi.fn().mockResolvedValue('replacement')
    const writeText = vi.fn().mockResolvedValue(undefined)

    await expect(
      executeEditCommand('paste', context, document, { readText, writeText }),
    ).resolves.toBe(false)
    await expect(
      executeEditCommand('copy', context, document, { readText, writeText }),
    ).resolves.toBe(false)

    expect(input.value).toBe('unchanged')
    expect(readText).not.toHaveBeenCalled()
    expect(writeText).not.toHaveBeenCalled()
  })

  it('copies an ordinary document selection through the Clipboard API', async () => {
    const paragraph = document.createElement('p')
    paragraph.textContent = 'selected page content'
    document.body.append(paragraph)
    const range = document.createRange()
    range.setStart(paragraph.firstChild!, 0)
    range.setEnd(paragraph.firstChild!, 8)
    const selection = document.getSelection()!
    selection.removeAllRanges()
    selection.addRange(range)
    const context = captureEditingContext()
    const writeText = vi.fn().mockResolvedValue(undefined)

    await expect(
      executeEditCommand('copy', context, document, {
        readText: vi.fn(),
        writeText,
      }),
    ).resolves.toBe(true)

    expect(writeText).toHaveBeenCalledWith('selected')
  })
})

function setDesktopMarker(platform: DesktopPlatform): void {
  Object.defineProperty(window, '__MORROW_DESKTOP__', {
    configurable: true,
    value: Object.freeze({ platform }),
  })
}

function clearDesktopMarker(): void {
  Reflect.deleteProperty(window, '__MORROW_DESKTOP__')
}
