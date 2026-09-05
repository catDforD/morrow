// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type {
  ClientMessage,
  CommandDefinition,
  SessionStreamFrame,
} from './types'
import { eventFrame, sessionEntry, snapshotFrame, turnProjection, testModelSelection } from './sessionTestFixtures'

const api = vi.hoisted(() => {
  class SessionProtocolError extends Error {}
  return {
    fetchJson: vi.fn(),
    SessionClient: class {},
    SessionProtocolError,
    sessionClient: {
      listSessions: vi.fn(),
      createSession: vi.fn(),
      resetSession: vi.fn(),
      archiveSession: vi.fn(),
      restoreSession: vi.fn(),
      getModelSelection: vi.fn(),
      setModelSelection: vi.fn(),
      connectSession: vi.fn(),
    },
  }
})

vi.mock('./api', () => api)

import App from './App'
import type {
  SessionConnection,
  SessionConnectionHandlers,
} from './api'

let root: Root | null = null
let handlers: SessionConnectionHandlers | null = null
let connection: TestConnection
let failingUrls: Set<string>
let commands: CommandDefinition[] = []

describe('App Session creation flow', () => {
  beforeEach(() => {
    ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
      .IS_REACT_ACT_ENVIRONMENT = true
    Object.defineProperty(window, 'matchMedia', {
      configurable: true,
      value: vi.fn().mockImplementation((query: string) => ({
        matches: false,
        media: query,
        onchange: null,
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
        addListener: vi.fn(),
        removeListener: vi.fn(),
        dispatchEvent: vi.fn(),
      })),
    })
    Object.defineProperty(Element.prototype, 'scrollIntoView', {
      configurable: true,
      value: vi.fn(),
    })
    history.replaceState({}, '', '/')
    localStorage.clear()
    handlers = null
    connection = new TestConnection()
    failingUrls = new Set()
    commands = []
    api.sessionClient.listSessions.mockReset().mockResolvedValue({
      schema_version: 1,
      sessions: [],
      diagnostics: [],
    })
    api.sessionClient.createSession
      .mockReset()
      .mockImplementation(async (name: string) => sessionEntry(name))
    api.sessionClient.getModelSelection
      .mockReset()
      .mockResolvedValue(testModelSelection)
    api.sessionClient.setModelSelection.mockReset()
    api.sessionClient.resetSession.mockReset()
    api.sessionClient.archiveSession.mockReset()
    api.sessionClient.restoreSession.mockReset()
    api.sessionClient.connectSession
      .mockReset()
      .mockImplementation(async (
        _name: string,
        nextHandlers: SessionConnectionHandlers,
      ) => {
        handlers = nextHandlers
        return connection
      })
    api.fetchJson.mockReset().mockImplementation(async (url: string) => {
      if (failingUrls.has(url)) throw new Error(`${url} unavailable`)
      switch (url) {
        case '/api/status':
          return {
            workspace_root: '/workspace/morrow',
            config_path: null,
            permissions: { mode: 'workspace_write', shell: 'prompt' },
            version: '0.4.0',
            model_ready: true,
            model_store_path: '/models.json',
            mcp_store_path: '/mcp.json',
            command_store_path: '/commands',
            subagent_store_path: '/subagents.json',
            config_diagnostics: [],
          }
        case '/api/model-settings':
          return {
            providers: [{
              id: 'provider-1',
              name: 'Provider',
              base_url: 'http://model.test',
              api_format: 'openai_chat_completions',
              enabled: true,
              read_only: false,
              api_key_configured: true,
              timeout_secs: 30,
              models: [{
                id: 'model-1',
                name: 'Model',
                context_window_tokens: 128_000,
                reserved_output_tokens: 8_000,
                supports_tools: true,
                reasoning_profile: 'none',
              }],
            }],
            default_selection: testModelSelection,
            model_ready: true,
            store_path: '/models.json',
          }
        case '/api/commands':
          return { commands, store_path: '/commands', diagnostics: [] }
        case '/api/hooks':
          return {
            schema_version: 1,
            user_config_path: '/home/test/.morrow/hooks.toml',
            project_config_path: '/workspace/morrow/.morrow/hooks.toml',
            trust_store_path: '/home/test/.morrow/hook-trust.json',
            project_fingerprint: null,
            project_trusted: false,
            hooks: [],
            diagnostics: [],
          }
        case '/api/subagent-settings':
          return {
            profiles: [],
            roles: [],
            store_path: '/subagents.json',
            min_profiles: 0,
            max_profiles: 8,
            max_avatar_bytes: 1024,
            accepted_avatar_types: [],
          }
        default:
          throw new Error(`Unexpected fetch: ${url}`)
      }
    })
  })

  afterEach(async () => {
    await act(async () => root?.unmount())
    root = null
    document.body.replaceChildren()
    vi.clearAllMocks()
    vi.useRealTimers()
  })

  it('clicks New task, creates, applies Snapshot, then enables Send', async () => {
    await renderApp()

    expect(document.body.textContent).toContain('从一个想法开始')
    expect(document.querySelector('.composer textarea')).toBeNull()

    await act(async () => {
      document.querySelector<HTMLButtonElement>('.home-create-task')?.click()
    })
    await setInput(
      document.querySelector<HTMLInputElement>('input[aria-label="新会话名称"]'),
      'task-one',
    )
    await act(async () => {
      document.querySelector<HTMLFormElement>('.session-create-row')?.requestSubmit()
      for (let index = 0; index < 6; index += 1) await Promise.resolve()
    })

    expect(api.sessionClient.createSession).toHaveBeenCalledWith('task-one')
    expect(api.sessionClient.connectSession).toHaveBeenCalledWith(
      'task-one',
      expect.any(Object),
    )
    expect(document.querySelector<HTMLTextAreaElement>('.composer textarea')?.disabled)
      .toBe(true)
    expect(document.querySelector<HTMLButtonElement>('[aria-label="发送"]')?.disabled)
      .toBe(true)

    await act(async () => {
      handlers?.onMessage(snapshotFrame('task-one'))
      for (let index = 0; index < 4; index += 1) await Promise.resolve()
    })
    const prompt = document.querySelector<HTMLTextAreaElement>('.composer textarea')
    expect(prompt?.disabled).toBe(false)
    await setInput(prompt, 'hello from the browser')
    const send = document.querySelector<HTMLButtonElement>('[aria-label="发送"]')
    expect(send?.disabled).toBe(false)

    await act(async () => send?.click())
    expect(connection.sent).toEqual([
      expect.objectContaining({
        type: 'start_turn',
        data: expect.objectContaining({ prompt: 'hello from the browser' }),
      }),
    ])
  })

  it('keeps IME and Shift+Enter out of command selection and submission', async () => {
    commands = [{ name: 'review', description: 'Review changes', argument_hint: '', prompt: 'Review changes' }]
    await openReadySession()
    const input = document.querySelector<HTMLTextAreaElement>('.composer textarea')!
    await setInput(input, '/rev')
    expect(document.querySelector('[role="listbox"]')).not.toBeNull()
    await act(async () => {
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', isComposing: true, bubbles: true }))
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', shiftKey: true, bubbles: true }))
    })
    expect(input.value).toBe('/rev')
    expect(connection.sent).toHaveLength(0)
    await act(async () => {
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }))
    })
    expect(input.value).toBe('/review ')
    expect(connection.sent).toHaveLength(0)
  })

  it('collapses the process on completion after the user expanded it while running', async () => {
    await openReadySession()
    const turn = {
      ...turnProjection('running'),
      steps: [{ id: 'model-1', kind: 'model_call' as const, status: 'running' as const, model_message: { role: 'assistant' as const, reasoning_content: '先确认界面结构。' } }],
    }
    await act(async () => handlers?.onMessage(eventFrame('task-one', 1, { type: 'turn_upserted', data: turn })))
    const toggle = () => document.querySelector<HTMLButtonElement>('.execution-toggle')!
    await act(async () => toggle().click())
    await act(async () => toggle().click())
    expect(toggle().getAttribute('aria-expanded')).toBe('true')
    await act(async () => handlers?.onMessage(eventFrame('task-one', 2, {
      type: 'turn_upserted',
      data: { ...turn, status: 'completed', steps: [{ ...turn.steps[0], status: 'completed' }], messages: [{ role: 'assistant', content: '正式回答。' }] },
    })))
    expect(toggle().getAttribute('aria-expanded')).toBe('false')
    expect(document.querySelector('.execution-thought')).toBeNull()
    expect(document.querySelector('.message-row.assistant')?.textContent).toContain('正式回答。')
    await act(async () => toggle().click())
    expect(document.querySelector('.execution-thought')?.textContent).toBe('先确认界面结构。')
  })

  it('keeps a reading position while messages arrive and resumes following on demand', async () => {
    await openReadySession()
    await act(async () => handlers?.onMessage(eventFrame('task-one', 1, {
      type: 'turn_upserted', data: turnProjection('completed'),
    })))
    const scroller = document.querySelector<HTMLDivElement>('.message-scroll')!
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 1800 },
      clientHeight: { configurable: true, value: 500 },
    })
    await act(async () => {
      scroller.scrollTop = 200
      scroller.dispatchEvent(new Event('scroll', { bubbles: true }))
      handlers?.onMessage(eventFrame('task-one', 2, {
        type: 'turn_upserted',
        data: { ...turnProjection('completed'), messages: [{ role: 'assistant', content: 'A longer answer' }] },
      }))
    })
    expect(scroller.scrollTop).toBe(200)
    const jump = document.querySelector<HTMLButtonElement>('[aria-label="回到底部"]')
    expect(jump).not.toBeNull()
    await act(async () => jump?.click())
    expect(scroller.scrollTop).toBe(1300)
    expect(document.querySelector('[aria-label="回到底部"]')).toBeNull()
  })

  it('keeps approval focus contained and submits only an explicit decision', async () => {
    await openReadySession()
    const input = document.querySelector<HTMLTextAreaElement>('.composer textarea')!
    input.focus()
    await act(async () => handlers?.onMessage(eventFrame('task-one', 1, {
      type: 'approvals_replaced',
      data: [{ id: 'approval-1', reason: 'Run tests', action: { kind: 'shell_command', command: 'cargo test', cwd: '/workspace', timeout_secs: 60 } }],
    })))
    const panel = document.querySelector<HTMLElement>('.approval-panel')!
    expect(document.activeElement).toBe(panel)
    expect(connection.sent).toHaveLength(0)
    const approve = panel.querySelector<HTMLButtonElement>('.approve-button')!
    approve.focus()
    await act(async () => approve.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true })))
    expect(document.activeElement).toBe(panel.querySelector('button'))
    await act(async () => approve.click())
    expect(connection.sent).toEqual([{ type: 'approval_decision', data: { request_id: 'approval-1', approved: true } }])
    await act(async () => handlers?.onMessage(eventFrame('task-one', 2, { type: 'approvals_replaced', data: [] })))
    expect(document.activeElement).toBe(input)
  })

  it('keeps task creation available when optional settings requests fail', async () => {
    failingUrls.add('/api/model-settings')
    failingUrls.add('/api/commands')
    failingUrls.add('/api/hooks')
    failingUrls.add('/api/subagent-settings')
    await renderApp()

    await act(async () => {
      document.querySelector<HTMLButtonElement>('.home-create-task')?.click()
    })
    await setInput(
      document.querySelector<HTMLInputElement>('input[aria-label="新会话名称"]'),
      'degraded-task',
    )
    await act(async () => {
      document.querySelector<HTMLFormElement>('.session-create-row')?.requestSubmit()
      for (let index = 0; index < 6; index += 1) await Promise.resolve()
      handlers?.onMessage(snapshotFrame('degraded-task'))
    })

    expect(api.sessionClient.createSession).toHaveBeenCalledWith('degraded-task')
    const prompt = document.querySelector<HTMLTextAreaElement>('.composer textarea')
    expect(prompt?.disabled).toBe(false)
    await setInput(prompt, 'still inspectable')
    expect(document.querySelector<HTMLButtonElement>('[aria-label="发送"]')?.disabled)
      .toBe(true)
  })

  it('shows reconnecting state without surfacing a raw WebSocket Event', async () => {
    await renderApp()

    await act(async () => {
      document.querySelector<HTMLButtonElement>('.home-create-task')?.click()
    })
    await setInput(
      document.querySelector<HTMLInputElement>('input[aria-label="新会话名称"]'),
      'task-one',
    )
    await act(async () => {
      document.querySelector<HTMLFormElement>('.session-create-row')?.requestSubmit()
      for (let index = 0; index < 6; index += 1) await Promise.resolve()
    })
    await act(async () => {
      handlers?.onMessage(snapshotFrame('task-one'))
      for (let index = 0; index < 4; index += 1) await Promise.resolve()
    })
    expect(document.querySelector<HTMLTextAreaElement>('.composer textarea')?.disabled)
      .toBe(false)

    await act(async () => {
      handlers?.onError(new Event('error'))
    })

    expect(document.body.textContent).toContain(
      '连接已中断，正在恢复会话',
    )
    expect(document.body.textContent).not.toContain('[object Event]')
  })

  it('dismisses the damaged task log banner until diagnostics change', async () => {
    const diagnostic = {
      name: 'broken-task',
      path: '/sessions/broken-task.jsonl',
      message: 'invalid JSON at line 3',
    }
    api.sessionClient.listSessions.mockReset().mockResolvedValue({
      schema_version: 1,
      sessions: [],
      diagnostics: [diagnostic],
    })
    await renderApp()

    expect(document.body.textContent).toContain(
      '已跳过 1 个损坏的会话记录，其余会话仍可使用。',
    )
    const dismiss = document.querySelector<HTMLButtonElement>(
      'button[title="关闭提示"]',
    )
    expect(dismiss).not.toBeNull()
    await act(async () => dismiss?.click())
    expect(document.body.textContent).not.toContain('损坏的会话记录')

    await act(async () => {
      document.querySelector<HTMLButtonElement>('.home-create-task')?.click()
    })
    await setInput(
      document.querySelector<HTMLInputElement>('input[aria-label="新会话名称"]'),
      'task-one',
    )
    await act(async () => {
      document.querySelector<HTMLFormElement>('.session-create-row')?.requestSubmit()
      for (let index = 0; index < 6; index += 1) await Promise.resolve()
    })
    await act(async () => {
      handlers?.onMessage(snapshotFrame('task-one'))
      for (let index = 0; index < 4; index += 1) await Promise.resolve()
    })

    // Refreshing the directory with identical diagnostics keeps it dismissed.
    const callsBeforeRefresh = api.sessionClient.listSessions.mock.calls.length
    await act(async () => {
      handlers?.onMessage(
        eventFrame('task-one', 1, {
          type: 'turn_upserted',
          data: turnProjection('completed'),
        }),
      )
      for (let index = 0; index < 6; index += 1) await Promise.resolve()
    })
    expect(api.sessionClient.listSessions.mock.calls.length).toBe(
      callsBeforeRefresh + 1,
    )
    expect(document.body.textContent).not.toContain('损坏的会话记录')

    // A changed diagnostics set has a new signature and re-shows the banner.
    api.sessionClient.listSessions.mockResolvedValue({
      schema_version: 1,
      sessions: [sessionEntry('task-one')],
      diagnostics: [
        diagnostic,
        {
          name: null,
          path: '/sessions/another-broken.jsonl',
          message: 'missing turn id',
        },
      ],
    })
    await act(async () => {
      handlers?.onMessage(
        eventFrame('task-one', 2, {
          type: 'turn_upserted',
          data: turnProjection('completed'),
        }),
      )
      for (let index = 0; index < 6; index += 1) await Promise.resolve()
    })
    expect(document.body.textContent).toContain(
      '已跳过 2 个损坏的会话记录，其余会话仍可使用。',
    )
  })
})

class TestConnection implements SessionConnection {
  open = true
  sent: ClientMessage[] = []

  get isOpen(): boolean {
    return this.open
  }

  send(message: ClientMessage): void {
    this.sent.push(message)
  }

  close(): void {
    this.open = false
  }
}

async function renderApp(): Promise<void> {
  const container = document.createElement('div')
  document.body.append(container)
  root = createRoot(container)
  await act(async () => {
    root?.render(<App />)
    for (let index = 0; index < 10; index += 1) await Promise.resolve()
  })
}

async function setInput(
  input: HTMLInputElement | HTMLTextAreaElement | null,
  value: string,
): Promise<void> {
  await act(async () => {
    if (!input) return
    const prototype = input instanceof HTMLTextAreaElement
      ? HTMLTextAreaElement.prototype
      : HTMLInputElement.prototype
    Object.getOwnPropertyDescriptor(prototype, 'value')?.set?.call(input, value)
    input.dispatchEvent(new Event('input', { bubbles: true }))
  })
}

async function openReadySession() {
  history.replaceState({}, '', '/?session=task-one')
  api.sessionClient.listSessions.mockResolvedValue({ schema_version: 1, sessions: [sessionEntry('task-one')], diagnostics: [] })
  await renderApp()
  await act(async () => {
    handlers?.onMessage(snapshotFrame('task-one'))
    for (let index = 0; index < 4; index += 1) await Promise.resolve()
  })
}
