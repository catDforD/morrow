// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from 'vitest'

import {
  BrowserTransport,
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
