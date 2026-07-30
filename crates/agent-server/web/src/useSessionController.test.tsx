// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  SessionClient,
  SessionProtocolError,
} from './api'
import type {
  SessionConnection,
  SessionConnectionHandlers,
} from './api'
import {
  eventFrame,
  sessionEntry,
  snapshotFrame,
  testModelSelection,
  turnProjection,
} from './sessionTestFixtures'
import {
  useSessionController,
} from './useSessionController'
import type { SessionController } from './useSessionController'
import type {
  ClientMessage,
  ModelSelection,
  SessionDirectoryResponse,
  SessionEntryResponse,
} from './types'

let root: Root | null = null
let controller: SessionController

describe('useSessionController', () => {
  beforeEach(() => {
    ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
      .IS_REACT_ACT_ENVIRONMENT = true
  })

  afterEach(async () => {
    await act(async () => root?.unmount())
    root = null
    document.body.replaceChildren()
    vi.useRealTimers()
  })

  it('keeps an empty directory in none without opening a default session', async () => {
    const client = new FakeSessionClient()
    await renderController(client, null)

    expect(controller.workspaceStatus).toBe('ready')
    expect(controller.selectionStatus).toBe('none')
    expect(controller.selected).toBeNull()
    expect(client.connections).toHaveLength(0)
  })

  it('allows explicit creation after the directory request fails', async () => {
    const client = new FakeSessionClient()
    client.listError = new Error('directory offline')
    await renderController(client, null)

    expect(controller.workspaceStatus).toBe('error')
    expect(controller.selectionStatus).toBe('none')
    client.listError = null
    await act(async () => {
      await controller.createSession('recovered')
    })

    expect(controller.workspaceStatus).toBe('ready')
    expect(controller.selected).toBe('recovered')
    expect(client.createdNames).toEqual(['recovered'])
  })

  it('does not fall back when the URL names a missing or archived session', async () => {
    const client = new FakeSessionClient([
      sessionEntry('active'),
      sessionEntry('archived', true),
    ])
    const notices: string[] = []
    await renderController(client, 'missing', (message) => notices.push(message))

    expect(controller.selected).toBeNull()
    expect(controller.selectionStatus).toBe('none')
    expect(client.connections).toHaveLength(0)
    expect(notices).toEqual(['Session "missing" is unavailable.'])
  })

  it('creates through REST and waits for Snapshot before becoming ready', async () => {
    const client = new FakeSessionClient()
    await renderController(client, null)

    await act(async () => {
      await controller.createSession('new_task')
    })

    expect(controller.workspaceStatus).toBe('ready')
    expect(controller.selected).toBe('new_task')
    expect(controller.selectionStatus).toBe('subscribing')
    expect(controller.timelineState.snapshot).toBeNull()
    expect(client.connections).toHaveLength(1)

    await act(async () => {
      client.connections[0].handlers.onMessage(snapshotFrame('new_task'))
    })

    expect(controller.selectionStatus).toBe('ready')
    expect(controller.timelineState.snapshot?.session_name).toBe('new_task')
  })

  it('ignores late frames and model-selection responses from an old generation', async () => {
    const client = new FakeSessionClient([
      sessionEntry('first'),
      sessionEntry('second'),
    ])
    client.deferModelSelectionFor.add('first')
    await renderController(client, 'first')
    const firstConnection = client.connections[0]

    await act(async () => {
      await controller.selectSession('second')
    })
    const secondConnection = client.connections[1]
    await act(async () => {
      secondConnection.handlers.onMessage(snapshotFrame('second'))
      client.resolveModelSelection('first', {
        provider_id: 'stale',
        model_id: 'stale',
        reasoning: 'max',
      })
      firstConnection.handlers.onMessage(snapshotFrame('first'))
      await Promise.resolve()
    })

    expect(controller.selected).toBe('second')
    expect(controller.timelineState.snapshot?.session_name).toBe('second')
    expect(controller.modelSelection).toEqual(testModelSelection)
  })

  it('reconnects from gaps and disconnects, then converges on a new Snapshot', async () => {
    vi.useFakeTimers()
    const client = new FakeSessionClient([sessionEntry('task-one')])
    await renderController(client, 'task-one')
    const first = client.connections[0]
    await act(async () => {
      first.handlers.onMessage(snapshotFrame('task-one'))
      first.handlers.onMessage(eventFrame('task-one', 1, {
        type: 'turn_upserted',
        data: turnProjection(),
      }))
      first.handlers.onMessage(eventFrame('task-one', 1, {
        type: 'notice',
        data: { message: 'duplicate' },
      }))
    })
    expect(controller.timelineState.snapshot?.cursor.sequence).toBe(1)

    await act(async () => {
      first.handlers.onMessage(eventFrame('task-one', 3, {
        type: 'notice',
        data: { message: 'gap' },
      }))
      await vi.runOnlyPendingTimersAsync()
      await Promise.resolve()
    })
    expect(client.connections).toHaveLength(2)
    expect(controller.selectionStatus).toBe('reconnecting')
    expect(controller.timelineState.snapshot?.session.turns).toHaveLength(1)

    const second = client.connections[1]
    await act(async () => {
      second.handlers.onMessage(snapshotFrame('task-one', 8))
    })
    expect(controller.selectionStatus).toBe('ready')
    expect(controller.timelineState.snapshot?.cursor.sequence).toBe(8)
    expect(controller.timelineState.snapshot?.session.turns).toHaveLength(0)

    await act(async () => {
      second.handlers.onClose()
      await vi.advanceTimersByTimeAsync(249)
    })
    expect(client.connections).toHaveLength(2)
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1)
      await Promise.resolve()
    })
    expect(client.connections).toHaveLength(3)
  })

  it('stops reconnecting on fatal protocol errors', async () => {
    vi.useFakeTimers()
    const client = new FakeSessionClient([sessionEntry('task-one')])
    await renderController(client, 'task-one')
    const connection = client.connections[0]

    await act(async () => {
      connection.handlers.onError(
        new SessionProtocolError('unsupported session stream v1'),
      )
      await vi.advanceTimersByTimeAsync(30_000)
    })

    expect(controller.selectionStatus).toBe('error')
    expect(controller.sessionError).toContain('unsupported session stream v1')
    expect(client.connections).toHaveLength(1)
  })

  it('returns to none after archiving the final active session', async () => {
    const client = new FakeSessionClient([sessionEntry('only')])
    await renderController(client, 'only')

    await act(async () => {
      await controller.archiveSession('only')
    })

    expect(controller.selected).toBeNull()
    expect(controller.selectionStatus).toBe('none')
    expect(client.connections).toHaveLength(1)
    expect(client.createdNames).toEqual([])
  })

  it('clears a pending turn when the transport rejects send synchronously', async () => {
    const client = new FakeSessionClient([sessionEntry('task-one')])
    await renderController(client, 'task-one')
    const connection = client.connections[0]
    await act(async () => {
      connection.handlers.onMessage(snapshotFrame('task-one'))
    })
    connection.throwOnSend = true

    expect(() => controller.send({
      type: 'start_turn',
      data: {
        request_id: 'request-1',
        prompt: 'hello',
        permission_mode: 'workspace_write',
        model_selection: testModelSelection,
      },
    })).toThrow('send failed')
    await act(async () => undefined)
    expect(controller.pendingTurnRequest).toBeNull()
  })
})

class FakeConnection implements SessionConnection {
  open = true
  closed = false
  throwOnSend = false
  sent: ClientMessage[] = []

  constructor(readonly handlers: SessionConnectionHandlers) {}

  get isOpen(): boolean {
    return this.open
  }

  send(message: ClientMessage): void {
    if (this.throwOnSend) throw new Error('send failed')
    this.sent.push(message)
  }

  close(): void {
    this.open = false
    this.closed = true
  }
}

class FakeSessionClient extends SessionClient {
  entries: SessionEntryResponse[]
  listError: Error | null = null
  connections: FakeConnection[] = []
  createdNames: string[] = []
  deferModelSelectionFor = new Set<string>()
  private modelResolvers = new Map<
    string,
    (selection: ModelSelection | null) => void
  >()

  constructor(entries: SessionEntryResponse[] = []) {
    super()
    this.entries = entries
  }

  override async listSessions(): Promise<SessionDirectoryResponse> {
    if (this.listError) throw this.listError
    return { schema_version: 1, sessions: [...this.entries], diagnostics: [] }
  }

  override async createSession(name: string): Promise<SessionEntryResponse> {
    this.createdNames.push(name)
    const entry = sessionEntry(name)
    this.entries.push(entry)
    return entry
  }

  override async resetSession(name: string): Promise<SessionEntryResponse> {
    return this.entries.find((entry) => entry.name === name) ?? sessionEntry(name)
  }

  override async archiveSession(name: string): Promise<SessionEntryResponse> {
    const entry = sessionEntry(name, true)
    this.entries = this.entries.map((current) =>
      current.name === name ? entry : current,
    )
    return entry
  }

  override async restoreSession(name: string): Promise<SessionEntryResponse> {
    const entry = sessionEntry(name)
    this.entries = this.entries.map((current) =>
      current.name === name ? entry : current,
    )
    return entry
  }

  override getModelSelection(name: string): Promise<ModelSelection | null> {
    if (!this.deferModelSelectionFor.has(name)) {
      return Promise.resolve(testModelSelection)
    }
    return new Promise((resolve) => this.modelResolvers.set(name, resolve))
  }

  resolveModelSelection(name: string, selection: ModelSelection | null): void {
    this.modelResolvers.get(name)?.(selection)
    this.modelResolvers.delete(name)
  }

  override async setModelSelection(
    _name: string,
    selection: ModelSelection,
  ): Promise<ModelSelection | null> {
    return selection
  }

  override async connectSession(
    _name: string,
    handlers: SessionConnectionHandlers,
  ): Promise<SessionConnection> {
    const connection = new FakeConnection(handlers)
    this.connections.push(connection)
    return connection
  }
}

async function renderController(
  client: SessionClient,
  initialSession: string | null,
  onNotice?: (message: string) => void,
): Promise<void> {
  const container = document.createElement('div')
  document.body.append(container)
  root = createRoot(container)
  await act(async () => {
    root?.render(
      <ControllerHarness
        client={client}
        initialSession={initialSession}
        onNotice={onNotice}
      />,
    )
    for (let index = 0; index < 8; index += 1) await Promise.resolve()
  })
}

function ControllerHarness({
  client,
  initialSession,
  onNotice,
}: {
  client: SessionClient
  initialSession: string | null
  onNotice?: (message: string) => void
}) {
  controller = useSessionController({ client, initialSession, onNotice })
  return <span>{controller.selectionStatus}</span>
}
