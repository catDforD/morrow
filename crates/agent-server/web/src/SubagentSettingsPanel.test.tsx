// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { SubagentRunStepIcon, findSubagentProfile } from './App'
import SubagentSettingsPanel, {
  normalizeSubagentAvatar,
} from './SubagentSettingsPanel'
import type {
  RunStep,
  SubagentProfileResponse,
  SubagentSettingsResponse,
} from './types'

let roots: Root[] = []

const profiles: SubagentProfileResponse[] = [
  { id: 'builtin-01', name: '后藤一里' },
  { id: 'builtin-02', name: '山田凉' },
  { id: 'builtin-03', name: '喜多郁代' },
  { id: 'builtin-04', name: '伊地知虹夏' },
]

const settings: SubagentSettingsResponse = {
  profiles,
  store_path: '/home/test/.morrow/subagents.json',
  min_profiles: 4,
  max_profiles: 64,
  max_avatar_bytes: 256 * 1024,
  accepted_avatar_types: ['image/png', 'image/jpeg', 'image/webp'],
}

describe('SubagentSettingsPanel', () => {
  beforeEach(() => {
    ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
      .IS_REACT_ACT_ENVIRONMENT = true
  })

  afterEach(async () => {
    await act(async () => {
      roots.forEach((root) => root.unmount())
    })
    roots = []
    document.body.replaceChildren()
    vi.restoreAllMocks()
    vi.unstubAllGlobals()
  })

  it('shows, searches and edits profiles while enforcing the four-profile floor', async () => {
    await render(<SubagentSettingsPanel settings={settings} onChanged={async () => {}} />)
    expect(document.body.textContent).toContain('后藤一里')
    expect(document.body.textContent).toContain('4 / 4 项')

    const search = document.querySelector<HTMLInputElement>('input[placeholder="搜索子智能体…"]')
    await setInput(search, '山田')
    expect(document.body.textContent).not.toContain('后藤一里')
    expect(document.body.textContent).toContain('山田凉')

    await act(async () => {
      document.querySelector<HTMLButtonElement>('.subagent-profile-row')?.click()
    })
    expect(document.body.textContent).toContain('编辑 山田凉')
    expect(document.querySelector<HTMLButtonElement>('.danger-button')?.disabled).toBe(true)
  })

  it('creates a profile through the server API', async () => {
    const fetchMock = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => new Response(
      JSON.stringify({ id: 'user-1', name: '新成员' }),
      { status: 200, headers: { 'content-type': 'application/json' } },
    ))
    vi.stubGlobal('fetch', fetchMock)
    const onChanged = vi.fn(async () => {})
    await render(<SubagentSettingsPanel settings={settings} onChanged={onChanged} />)

    await act(async () => {
      findButton('新建子智能体')?.click()
    })
    await setInput(document.querySelector<HTMLInputElement>('input[placeholder="输入 1–40 个字符"]'), '新成员')
    await act(async () => {
      document.querySelector<HTMLFormElement>('form')?.requestSubmit()
    })

    expect(fetchMock).toHaveBeenCalledOnce()
    const [url, options] = fetchMock.mock.calls[0]
    expect(url).toBe('/api/subagents')
    expect(options?.method).toBe('POST')
    expect(JSON.parse(String(options?.body))).toEqual({
      name: '新成员',
      avatar_data_url: null,
    })
    expect(onChanged).toHaveBeenCalledOnce()
  })

  it('requires confirmation before restoring defaults', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true)
    const fetchMock = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => new Response(
      JSON.stringify(settings),
      { status: 200, headers: { 'content-type': 'application/json' } },
    ))
    vi.stubGlobal('fetch', fetchMock)
    const onChanged = vi.fn(async () => {})
    await render(<SubagentSettingsPanel settings={settings} onChanged={onChanged} />)

    await act(async () => {
      findButton('恢复默认名单')?.click()
    })

    expect(window.confirm).toHaveBeenCalledOnce()
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/subagent-settings/reset',
      expect.objectContaining({ method: 'POST' }),
    )
    expect(onChanged).toHaveBeenCalledOnce()
  })
})

describe('subagent avatar behavior', () => {
  const avatar = 'data:image/webp;base64,AAAA'

  afterEach(async () => {
    await act(async () => {
      roots.forEach((root) => root.unmount())
    })
    roots = []
    document.body.replaceChildren()
    vi.restoreAllMocks()
    vi.unstubAllGlobals()
  })

  it('uses stable IDs first and names only for legacy history', () => {
    const current = [
      { id: 'builtin-01', name: '波奇', avatar_data_url: avatar },
      { id: 'user-2', name: '后藤一里', avatar_data_url: 'data:image/png;base64,BBBB' },
    ]
    expect(findSubagentProfile(current, 'builtin-01', '后藤一里')?.name).toBe('波奇')
    expect(findSubagentProfile(current, 'deleted-id', '后藤一里')).toBeUndefined()
    expect(findSubagentProfile(current, undefined, '后藤一里')?.id).toBe('user-2')
  })

  it.each(['running', 'ok', 'error'] as const)('shows the configured avatar while %s', async (status) => {
    const step: RunStep = {
      id: `call-${status}`,
      kind: 'subagent',
      status,
      title: '子智能体 · 后藤一里',
      agentId: 'builtin-01',
      agentName: '后藤一里',
    }
    await render(
      <SubagentRunStepIcon
        step={step}
        profiles={[{ id: 'builtin-01', name: '波奇', avatar_data_url: avatar }]}
      />,
    )
    expect(document.querySelector('img')?.getAttribute('src')).toBe(avatar)
  })

  it('falls back to the robot icon when an avatar fails to load', async () => {
    const step: RunStep = {
      id: 'call-broken',
      kind: 'subagent',
      status: 'error',
      title: '子智能体 · 后藤一里',
      agentId: 'builtin-01',
    }
    await render(
      <SubagentRunStepIcon
        step={step}
        profiles={[{ id: 'builtin-01', name: '波奇', avatar_data_url: avatar }]}
      />,
    )
    await act(async () => {
      document.querySelector('img')?.dispatchEvent(new Event('error'))
    })
    expect(document.querySelector('img')).toBeNull()
    expect(document.querySelector('svg')).not.toBeNull()
  })

  it('center-crops to 256px and prefers WebP', async () => {
    const originalImage = globalThis.Image
    class TestImage {
      naturalWidth = 400
      naturalHeight = 200
      onload: (() => void) | null = null
      onerror: (() => void) | null = null
      set src(_value: string) {
        queueMicrotask(() => this.onload?.())
      }
    }
    vi.stubGlobal('Image', TestImage as unknown as typeof Image)
    const drawImage = vi.fn()
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue({ drawImage } as unknown as CanvasRenderingContext2D)
    vi.spyOn(HTMLCanvasElement.prototype, 'toDataURL').mockReturnValue(avatar)

    const result = await normalizeSubagentAvatar(
      new File([new Uint8Array([1, 2, 3])], 'avatar.png', { type: 'image/png' }),
      256 * 1024,
    )

    expect(result).toBe(avatar)
    expect(drawImage).toHaveBeenCalledWith(
      expect.any(TestImage),
      100,
      0,
      200,
      200,
      0,
      0,
      256,
      256,
    )
    vi.stubGlobal('Image', originalImage)
  })

  it('rejects SVG before attempting image decoding', async () => {
    await expect(normalizeSubagentAvatar(
      new File(['<svg/>'], 'avatar.svg', { type: 'image/svg+xml' }),
      256 * 1024,
    )).rejects.toThrow('不接受 SVG 或 GIF')
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

async function setInput(input: HTMLInputElement | null, value: string): Promise<void> {
  if (!input) throw new Error('input not found')
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set
    setter?.call(input, value)
    input.dispatchEvent(new Event('input', { bubbles: true }))
  })
}

function findButton(label: string): HTMLButtonElement | undefined {
  return [...document.querySelectorAll<HTMLButtonElement>('button')]
    .find((button) => button.textContent?.includes(label))
}
