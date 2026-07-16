import { Channel, invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

export type DesktopPlatform = 'windows' | 'macos' | 'linux'

export interface RecentWorkspace {
  index: number
  label: string
  target: string
  path: string
}

export type WorkspaceLocation =
  | { kind: 'local'; path: string }
  | { kind: 'wsl'; distro: string; user: string; path: string }

export interface DesktopShellState {
  isMaximized: boolean
  recentWorkspaces: RecentWorkspace[]
  activeWorkspace: WorkspaceLocation | null
}

export interface WslDistribution {
  name: string
  version: number
  is_default: boolean
}

export interface WslProbe {
  distro: string
  user: string
  home: string
  arch: string
}

export type RemoteRequest =
  | { type: 'ping' }
  | { type: 'activity' }
  | { type: 'environment' }
  | { type: 'list_directory'; data: { path?: string; show_hidden: boolean } }
  | { type: 'http'; data: { method: string; path: string; body?: unknown } }
  | { type: 'subscribe_session'; data: { session: string } }
  | { type: 'unsubscribe_session'; data: { subscription_id: string } }
  | { type: 'session_message'; data: { session: string; message: unknown } }

export type RemoteResponse =
  | { type: 'pong' }
  | { type: 'ack' }
  | { type: 'activity'; data: { running_turns: number; pending_approvals: number } }
  | {
      type: 'directory'
      data: {
        path: string
        parent?: string
        entries: Array<{
          name: string
          path: string
          directory: boolean
          hidden: boolean
        }>
      }
    }
  | { type: 'http'; data: { status: number; body?: unknown } }
  | {
      type: 'session_subscribed'
      data: { subscription_id: string; snapshot: unknown }
    }
  | { type: 'error'; data: { code: string; message: string } }

export interface RemoteEnvelope {
  protocol_version: number
  channel_id: number
  request_id: string
  message: {
    type: 'event'
    data:
      | {
          type: 'session_message'
          data: { subscription_id: string; message: unknown }
        }
      | { type: 'workspace_log'; data: { level: string; message: string } }
      | { type: 'worker_exited'; data: { channel_id: number; code?: number } }
      | { type: 'workspace_reconnected'; data: { channel_id: number } }
  }
}

export type DesktopAction =
  | { type: 'start_drag' }
  | { type: 'minimize' }
  | { type: 'toggle_maximize' }
  | { type: 'close_window' }
  | { type: 'quit' }
  | { type: 'open_folder' }
  | { type: 'open_recent'; index: number }
  | { type: 'open_logs' }
  | { type: 'download_latest' }

export type EditCommand =
  | 'undo'
  | 'redo'
  | 'cut'
  | 'copy'
  | 'paste'
  | 'select_all'

type TextControl = HTMLInputElement | HTMLTextAreaElement

export type EditingContext =
  | {
      kind: 'text'
      element: TextControl
      start: number
      end: number
      direction: 'forward' | 'backward' | 'none'
    }
  | {
      kind: 'selection'
      activeElement: HTMLElement | null
      ranges: Range[]
    }
  | {
      kind: 'none'
      activeElement: HTMLElement | null
    }

export function getDesktopPlatform(): DesktopPlatform | null {
  const platform = window.__MORROW_DESKTOP__?.platform
  return platform === 'windows' ||
    platform === 'macos' ||
    platform === 'linux'
    ? platform
    : null
}

export async function getDesktopShellState(): Promise<DesktopShellState> {
  return invoke<DesktopShellState>('desktop_shell_state')
}

export async function runDesktopAction(action: DesktopAction): Promise<void> {
  await invoke('desktop_action', { action })
}

export async function listWslDistributions(): Promise<WslDistribution[]> {
  return invoke<WslDistribution[]>('desktop_wsl_distributions')
}

export async function probeWsl(distro: string, user: string): Promise<WslProbe> {
  return invoke<WslProbe>('desktop_wsl_probe', { distro, user })
}

export async function prepareWsl(distro: string, user: string): Promise<WslProbe> {
  return invoke<WslProbe>('desktop_wsl_prepare', { distro, user })
}

export async function connectWsl(
  distro: string,
  user: string,
  path: string,
): Promise<void> {
  await invoke('desktop_wsl_connect', { distro, user, path })
}

export async function remoteRequest<T extends RemoteResponse>(
  request: RemoteRequest,
): Promise<T> {
  const response = await invoke<T>('desktop_remote_request', { request })
  if (response.type === 'error') throw new Error(response.data.message)
  return response
}

export function listenRemoteEvents(
  listener: (envelope: RemoteEnvelope) => void,
): Promise<() => void> {
  const channel = new Channel<RemoteEnvelope>()
  channel.onmessage = listener
  return invoke<number>('desktop_remote_subscribe', { onEvent: channel }).then(
    (subscriptionId) => {
      let active = true
      return () => {
        if (!active) return
        active = false
        channel.onmessage = () => undefined
        void invoke('desktop_remote_unsubscribe', { subscriptionId })
      }
    },
  )
}

export function listenWslLogs(listener: (message: string) => void): Promise<() => void> {
  return listen<string>('morrow-wsl-log', (event) => listener(event.payload))
}

export function captureEditingContext(
  documentRef: Document = document,
): EditingContext {
  const activeElement =
    documentRef.activeElement instanceof HTMLElement
      ? documentRef.activeElement
      : null

  if (isTextControl(activeElement)) {
    const start = activeElement.selectionStart
    const end = activeElement.selectionEnd
    if (start !== null && end !== null) {
      return {
        kind: 'text',
        element: activeElement,
        start,
        end,
        direction: activeElement.selectionDirection ?? 'none',
      }
    }
  }

  const selection = documentRef.getSelection()
  if (selection && selection.rangeCount > 0) {
    return {
      kind: 'selection',
      activeElement,
      ranges: Array.from({ length: selection.rangeCount }, (_, index) =>
        selection.getRangeAt(index).cloneRange(),
      ),
    }
  }

  return { kind: 'none', activeElement }
}

export function restoreEditingContext(
  context: EditingContext,
  documentRef: Document = document,
): void {
  if (context.kind === 'text') {
    if (!context.element.isConnected) return
    context.element.focus({ preventScroll: true })
    context.element.setSelectionRange(
      context.start,
      context.end,
      context.direction,
    )
    return
  }

  context.activeElement?.focus({ preventScroll: true })
  if (context.kind !== 'selection') return

  const selection = documentRef.getSelection()
  if (!selection) return
  selection.removeAllRanges()
  for (const range of context.ranges) {
    if (range.startContainer.isConnected && range.endContainer.isConnected) {
      selection.addRange(range)
    }
  }
}

export async function executeEditCommand(
  command: EditCommand,
  context: EditingContext,
  documentRef: Document = document,
  clipboard: Pick<Clipboard, 'readText' | 'writeText'> | undefined =
    navigator.clipboard,
): Promise<boolean> {
  restoreEditingContext(context, documentRef)

  if (command === 'undo' || command === 'redo') {
    return documentRef.execCommand?.(command) ?? false
  }

  if (command === 'select_all') {
    if (context.kind === 'text') {
      context.element.select()
      return true
    }
    const selection = documentRef.getSelection()
    if (!selection) return false
    selection.selectAllChildren(documentRef.body)
    return true
  }

  if (
    context.kind === 'text' &&
    (context.element.disabled || context.element.readOnly) &&
    (command === 'cut' || command === 'paste')
  ) {
    return false
  }

  if (command === 'copy' || command === 'cut') {
    const text = selectedText(context, documentRef)
    if (!text) return false
    if (documentRef.execCommand?.(command)) return true
    if (!clipboard) return false
    try {
      await clipboard.writeText(text)
    } catch {
      return documentRef.execCommand?.(command) ?? false
    }

    if (command === 'cut') {
      if (context.kind === 'text') {
        replaceTextControlSelection(context, '', 'deleteByCut')
      } else if (context.kind === 'selection' && selectionIsEditable(context)) {
        const selection = documentRef.getSelection()
        selection?.deleteFromDocument()
        context.activeElement?.dispatchEvent(createInputEvent('deleteByCut'))
      }
    }
    return true
  }

  if (!clipboard) return documentRef.execCommand?.('paste') ?? false

  try {
    const text = await clipboard.readText()
    if (documentRef.execCommand?.('insertText', false, text)) return true
    if (context.kind === 'text') {
      replaceTextControlSelection(context, text, 'insertFromPaste')
      return true
    }
    if (context.kind === 'selection' && selectionIsEditable(context)) {
      insertSelectionText(documentRef, context.activeElement, text)
      return true
    }
    return documentRef.execCommand?.('insertText', false, text) ?? false
  } catch {
    return documentRef.execCommand?.('paste') ?? false
  }
}

function isTextControl(element: Element | null): element is TextControl {
  if (element instanceof HTMLTextAreaElement) return true
  if (!(element instanceof HTMLInputElement)) return false
  return ['text', 'search', 'url', 'tel', 'password', 'email'].includes(
    element.type,
  )
}

function selectedText(
  context: EditingContext,
  documentRef: Document,
): string {
  if (context.kind === 'text') {
    return context.element.value.slice(context.start, context.end)
  }
  if (context.kind === 'selection') {
    return documentRef.getSelection()?.toString() ?? ''
  }
  return ''
}

function replaceTextControlSelection(
  context: Extract<EditingContext, { kind: 'text' }>,
  replacement: string,
  inputType: 'deleteByCut' | 'insertFromPaste',
): void {
  const { element, start, end } = context
  const nextValue = `${element.value.slice(0, start)}${replacement}${element.value.slice(end)}`
  const prototype =
    element instanceof HTMLTextAreaElement
      ? HTMLTextAreaElement.prototype
      : HTMLInputElement.prototype
  const nativeSetter = Object.getOwnPropertyDescriptor(prototype, 'value')?.set
  nativeSetter?.call(element, nextValue)
  const caret = start + replacement.length
  element.setSelectionRange(caret, caret, 'none')
  element.dispatchEvent(createInputEvent(inputType, replacement))
}

function selectionIsEditable(
  context: Extract<EditingContext, { kind: 'selection' }>,
): boolean {
  if (context.activeElement?.isContentEditable) return true
  return context.ranges.some((range) => {
    const node =
      range.commonAncestorContainer instanceof Element
        ? range.commonAncestorContainer
        : range.commonAncestorContainer.parentElement
    return Boolean(node?.closest('[contenteditable="true"]'))
  })
}

function insertSelectionText(
  documentRef: Document,
  activeElement: HTMLElement | null,
  text: string,
): void {
  const selection = documentRef.getSelection()
  const range = selection?.rangeCount ? selection.getRangeAt(0) : null
  if (!selection || !range) return
  range.deleteContents()
  const node = documentRef.createTextNode(text)
  range.insertNode(node)
  range.setStartAfter(node)
  range.collapse(true)
  selection.removeAllRanges()
  selection.addRange(range)
  activeElement?.dispatchEvent(createInputEvent('insertFromPaste', text))
}

function createInputEvent(inputType: string, data: string | null = null): Event {
  try {
    return new InputEvent('input', {
      bubbles: true,
      composed: true,
      data,
      inputType,
    })
  } catch {
    return new Event('input', { bubbles: true, composed: true })
  }
}
