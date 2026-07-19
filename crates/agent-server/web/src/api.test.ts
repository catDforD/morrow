// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from 'vitest'

const bridge = vi.hoisted(() => ({
  getDesktopPlatform: vi.fn(),
  getDesktopShellState: vi.fn(),
  listenRemoteEvents: vi.fn(),
  remoteRequest: vi.fn(),
}))

vi.mock('./desktop', () => bridge)

import { BrowserTransport, DesktopTransport } from './api'

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
          data: { subscription_id: 'subscription-1', snapshot: { type: 'snapshot' } },
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
    const connection = await transport.openSessionConnection('default', handlers)

    expect(handlers.onMessage).toHaveBeenCalledWith({ type: 'snapshot' })
    expect(handlers.onOpen).toHaveBeenCalledOnce()
    connection.send(JSON.stringify({ type: 'cancel_turn', data: { turn_id: 'turn-1' } }))
    await Promise.resolve()
    expect(bridge.remoteRequest).toHaveBeenCalledWith({
      type: 'session_message',
      data: {
        session: 'default',
        message: { type: 'cancel_turn', data: { turn_id: 'turn-1' } },
      },
    })

    eventListener?.({
      message: {
        data: {
          type: 'session_message',
          data: { subscription_id: 'subscription-1', message: { type: 'turn_saved' } },
        },
      },
    })
    expect(handlers.onMessage).toHaveBeenLastCalledWith({ type: 'turn_saved' })

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
    await Promise.resolve()
    expect(handlers.onOpen).toHaveBeenCalledTimes(2)

    connection.close()
    await Promise.resolve()
    expect(unlisten).toHaveBeenCalledOnce()
    expect(handlers.onClose).toHaveBeenCalledTimes(2)
  })
})
