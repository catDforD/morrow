import {
  getDesktopPlatform,
  getDesktopShellState,
  listenRemoteEvents,
  remoteRequest,
} from './desktop'

export interface SessionConnection {
  isOpen: boolean
  send(message: string): void
  close(): void
}

export interface SessionConnectionHandlers {
  onOpen(): void
  onClose(): void
  onMessage(message: unknown): void
  onError(error: unknown): void
}

export interface AppTransport {
  fetchJson<T>(url: string, options?: RequestInit): Promise<T>
  openSessionConnection(
    name: string,
    handlers: SessionConnectionHandlers,
  ): Promise<SessionConnection>
}

export class BrowserTransport implements AppTransport {
  async fetchJson<T>(url: string, options?: RequestInit): Promise<T> {
    const response = await fetch(url, options)
    if (!response.ok) {
      const body = await response.json().catch(() => ({}))
      const message =
        typeof body.error === 'string'
          ? body.error
          : `${response.status} ${response.statusText}`
      throw new Error(message)
    }
    if (response.status === 204) return undefined as T
    return response.json() as Promise<T>
  }

  async openSessionConnection(
    name: string,
    handlers: SessionConnectionHandlers,
  ): Promise<SessionConnection> {
    const socket = new WebSocket(sessionSocketUrl(name))
    const connection: SessionConnection = {
      get isOpen() {
        return socket.readyState === WebSocket.OPEN
      },
      send(message) {
        socket.send(message)
      },
      close() {
        socket.close()
      },
    }
    socket.addEventListener('open', handlers.onOpen)
    socket.addEventListener('close', handlers.onClose)
    socket.addEventListener('error', handlers.onError)
    socket.addEventListener('message', (event) => {
      try {
        handlers.onMessage(JSON.parse(event.data))
      } catch (error) {
        handlers.onError(error)
      }
    })
    return connection
  }
}

export class DesktopTransport implements AppTransport {
  async fetchJson<T>(url: string, options?: RequestInit): Promise<T> {
    const rawBody = options?.body
    const body =
      typeof rawBody === 'string' && rawBody.length > 0
        ? JSON.parse(rawBody)
        : undefined
    const response = await remoteRequest<{
      type: 'http'
      data: { status: number; body?: unknown }
    }>({
      type: 'http',
      data: {
        method: options?.method ?? 'GET',
        path: url,
        body,
      },
    })
    if (response.data.status < 200 || response.data.status >= 300) {
      const errorBody = response.data.body as { error?: string } | undefined
      throw new Error(errorBody?.error ?? `Remote request failed: ${response.data.status}`)
    }
    return response.data.body as T
  }

  async openSessionConnection(
    name: string,
    handlers: SessionConnectionHandlers,
  ): Promise<SessionConnection> {
    let open = true
    let closedByUser = false
    let subscriptionId = ''
    const subscribe = async () => {
      const response = await remoteRequest<{
        type: 'session_subscribed'
        data: { subscription_id: string; snapshot: unknown }
      }>({ type: 'subscribe_session', data: { session: name } })
      if (closedByUser) return
      subscriptionId = response.data.subscription_id
      handlers.onMessage(response.data.snapshot)
      open = true
      handlers.onOpen()
    }
    const unlisten = await listenRemoteEvents((envelope) => {
      const event = envelope.message.data
      if (
        event.type === 'session_message' &&
        event.data.subscription_id === subscriptionId
      ) {
        handlers.onMessage(event.data.message)
      } else if (event.type === 'worker_exited') {
        open = false
        handlers.onClose()
      } else if (event.type === 'workspace_reconnected' && !closedByUser) {
        void subscribe().catch(handlers.onError)
      }
    })
    await subscribe()

    return {
      get isOpen() {
        return open
      },
      send(message) {
        if (!open) throw new Error('remote session is not connected')
        void remoteRequest({
          type: 'session_message',
          data: { session: name, message: JSON.parse(message) },
        }).catch(handlers.onError)
      },
      close() {
        if (closedByUser) return
        closedByUser = true
        open = false
        unlisten()
        void remoteRequest({
          type: 'unsubscribe_session',
          data: { subscription_id: subscriptionId },
        }).finally(handlers.onClose)
      },
    }
  }
}

const browserTransport = new BrowserTransport()
const desktopTransport = new DesktopTransport()

async function currentTransport(): Promise<AppTransport> {
  if (!getDesktopPlatform()) return browserTransport
  try {
    return (await getDesktopShellState()).activeWorkspace?.kind === 'wsl'
      ? desktopTransport
      : browserTransport
  } catch {
    return browserTransport
  }
}

export async function fetchJson<T>(url: string, options?: RequestInit): Promise<T> {
  return (await currentTransport()).fetchJson<T>(url, options)
}

export function sessionSocketUrl(name: string): string {
  const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${protocol}//${location.host}/api/sessions/${encodeURIComponent(name)}/ws`
}

export async function openSessionConnection(
  name: string,
  handlers: SessionConnectionHandlers,
): Promise<SessionConnection> {
  return (await currentTransport()).openSessionConnection(name, handlers)
}
