// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from 'vitest'

const bridge = vi.hoisted(() => ({
  getDesktopPlatform: vi.fn(),
  getDesktopShellState: vi.fn(),
  listenRemoteEvents: vi.fn(),
  remoteRequest: vi.fn(),
}))

vi.mock('./desktop', () => bridge)

import {
  BrowserTransport,
  DesktopTransport,
  SessionClient,
  SessionProtocolError,
  parseSessionStreamFrame,
} from './api'
import {
  eventFrame,
  sessionEntry,
  snapshotFrame,
} from './sessionTestFixtures'

describe('BrowserTransport', () => {
  beforeEach(() => {
    vi.restoreAllMocks()
  })

  it('keeps REST response and error behavior', async () => {
    const fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ value: 7 }), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        }),
      )
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ error: 'broken' }), {
          status: 409,
          headers: { 'content-type': 'application/json' },
        }),
      )
    const transport = new BrowserTransport()

    await expect(transport.fetchJson<{ value: number }>('/api/value')).resolves.toEqual({
      value: 7,
    })
    await expect(transport.fetchJson('/api/value')).rejects.toThrow('broken')
    expect(fetchMock).toHaveBeenCalledTimes(2)
  })

  it('runs the typed SessionClient directory and create contract', async () => {
    vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(
        new Response(JSON.stringify({
          schema_version: 1,
          sessions: [],
          diagnostics: [],
        }), { status: 200 }),
      )
      .mockResolvedValueOnce(
        new Response(JSON.stringify(sessionEntry('browser-task')), {
          status: 201,
        }),
      )
    const client = new SessionClient(new BrowserTransport())

    await expect(client.listSessions()).resolves.toMatchObject({ sessions: [] })
    await expect(client.createSession('browser-task')).resolves.toMatchObject({
      name: 'browser-task',
    })
    expect(globalThis.fetch).toHaveBeenLastCalledWith('/api/sessions', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name: 'browser-task' }),
    })
  })
})

describe('DesktopTransport', () => {
  beforeEach(() => {
    bridge.remoteRequest.mockReset()
    bridge.listenRemoteEvents.mockReset()
  })

  it('maps HTTP calls onto framed remote requests', async () => {
    bridge.remoteRequest.mockResolvedValue({
      type: 'http',
      data: { status: 200, body: { ok: true } },
    })
    const transport = new DesktopTransport()

    await expect(
      transport.fetchJson('/api/model-settings', {
        method: 'POST',
        body: JSON.stringify({ enabled: true }),
      }),
    ).resolves.toEqual({ ok: true })
    expect(bridge.remoteRequest).toHaveBeenCalledWith({
      type: 'http',
      data: {
        method: 'POST',
        path: '/api/model-settings',
        body: { enabled: true },
      },
    })
  })

  it('runs the typed SessionClient directory and create contract', async () => {
    bridge.remoteRequest
      .mockResolvedValueOnce({
        type: 'http',
        data: {
          status: 200,
          body: { schema_version: 1, sessions: [], diagnostics: [] },
        },
      })
      .mockResolvedValueOnce({
        type: 'http',
        data: { status: 201, body: sessionEntry('desktop-task') },
      })
    const client = new SessionClient(new DesktopTransport())

    await expect(client.listSessions()).resolves.toMatchObject({ sessions: [] })
    await expect(client.createSession('desktop-task')).resolves.toMatchObject({
      name: 'desktop-task',
    })
    expect(bridge.remoteRequest).toHaveBeenLastCalledWith({
      type: 'http',
      data: {
        method: 'POST',
        path: '/api/sessions',
        body: { name: 'desktop-task' },
      },
    })
  })

  it('subscribes, streams, sends, and closes a remote session', async () => {
    let eventListener: ((envelope: unknown) => void) | undefined
    const unlisten = vi.fn()
    bridge.listenRemoteEvents.mockImplementation(async (listener) => {
      eventListener = listener
      return unlisten
    })
    bridge.remoteRequest.mockImplementation(async (request) => {
      if (request.type === 'subscribe_session') {
        return {
          type: 'session_subscribed',
          data: {
            subscription_id: request.data.subscription_id,
            snapshot: snapshotFrame('task-one'),
          },
        }
      }
      return { type: 'ack' }
    })
    const handlers = {
      onOpen: vi.fn(),
      onClose: vi.fn(),
      onMessage: vi.fn(),
      onError: vi.fn(),
    }
    const transport = new DesktopTransport()
    const connection = await transport.openSessionConnection('task-one', handlers)
    const firstSubscriptionId = bridge.remoteRequest.mock.calls.find(
      ([request]) => request.type === 'subscribe_session',
    )?.[0].data.subscription_id

    expect(handlers.onMessage).toHaveBeenCalledWith(snapshotFrame('task-one'))
    expect(handlers.onOpen).toHaveBeenCalledOnce()
    connection.send({ type: 'cancel_turn', data: { turn_id: 'turn-1' } })
    await Promise.resolve()
    expect(bridge.remoteRequest).toHaveBeenCalledWith({
      type: 'session_message',
      data: {
        session: 'task-one',
        message: { type: 'cancel_turn', data: { turn_id: 'turn-1' } },
      },
    })

    eventListener?.({
      message: {
        data: {
          type: 'session_message',
          data: {
            subscription_id: firstSubscriptionId,
            message: eventFrame('task-one', 1, {
              type: 'notice',
              data: { message: 'updated' },
            }),
          },
        },
      },
    })
    expect(handlers.onMessage).toHaveBeenLastCalledWith(
      eventFrame('task-one', 1, {
        type: 'notice',
        data: { message: 'updated' },
      }),
    )

    eventListener?.({
      message: {
        data: { type: 'worker_exited', data: { channel_id: 1, code: 1 } },
      },
    })
    expect(handlers.onClose).toHaveBeenCalledOnce()
    eventListener?.({
      message: {
        data: { type: 'workspace_reconnected', data: { channel_id: 2 } },
      },
    })
    await Promise.resolve()
    expect(handlers.onOpen).toHaveBeenCalledOnce()

    connection.close()
    await Promise.resolve()
    expect(unlisten).toHaveBeenCalledOnce()
    expect(handlers.onClose).toHaveBeenCalledOnce()
  })

  it('buffers events that arrive before the subscribe response snapshot', async () => {
    let eventListener: ((envelope: any) => void) | undefined
    let finishSubscribe: ((value: unknown) => void) | undefined
    bridge.listenRemoteEvents.mockImplementation(async (listener) => {
      eventListener = listener
      return vi.fn()
    })
    bridge.remoteRequest.mockImplementation((request) => {
      if (request.type !== 'subscribe_session') return Promise.resolve({ type: 'ack' })
      return new Promise((resolve) => {
        finishSubscribe = resolve
      })
    })
    const handlers = {
      onOpen: vi.fn(),
      onClose: vi.fn(),
      onMessage: vi.fn(),
      onError: vi.fn(),
    }
    const opening = new DesktopTransport().openSessionConnection('task-one', handlers)
    await Promise.resolve()
    await Promise.resolve()
    const request = bridge.remoteRequest.mock.calls[0][0]
    eventListener?.({
      message: {
        data: {
          type: 'session_message',
          data: {
            subscription_id: request.data.subscription_id,
            message: eventFrame('task-one', 1, {
              type: 'notice',
              data: { message: 'early' },
            }),
          },
        },
      },
    })
    finishSubscribe?.({
      type: 'session_subscribed',
      data: {
        subscription_id: request.data.subscription_id,
        snapshot: snapshotFrame('task-one'),
      },
    })
    await opening

    expect(handlers.onMessage.mock.calls.map(([message]) => message.type)).toEqual([
      'snapshot',
      'event',
    ])
  })
})

describe('session protocol parsing', () => {
  it('rejects malformed snapshots and incompatible versions as fatal', () => {
    const malformed = snapshotFrame('task-one')
    if (malformed.type === 'snapshot') {
      malformed.data.session.diagnostics = undefined as never
    }
    expect(() => parseSessionStreamFrame(malformed, 'task-one')).toThrow(
      SessionProtocolError,
    )

    const incompatible = snapshotFrame('task-one')
    if (incompatible.type === 'snapshot') incompatible.data.schema_version = 1
    expect(() => parseSessionStreamFrame(incompatible, 'task-one')).toThrow(
      'unsupported session stream v1',
    )
  })

  it('rejects mismatched session identity and unsupported updates', () => {
    expect(() => parseSessionStreamFrame(snapshotFrame('other'), 'task-one')).toThrow(
      'snapshot session mismatch',
    )
    const frame = eventFrame('task-one', 1, {
      type: 'notice',
      data: { message: 'ok' },
    })
    if (frame.type === 'event') {
      frame.data.update = { type: 'legacy_turn_saved', data: {} } as never
    }
    expect(() => parseSessionStreamFrame(frame, 'task-one')).toThrow(
      'unsupported session update legacy_turn_saved',
    )
  })
})
