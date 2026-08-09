// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { fetchJson } from './api'
import HookSettingsPanel from './HookSettingsPanel'
import type { HookSettingsResponse } from './types'

vi.mock('./api', () => ({ fetchJson: vi.fn() }))

let root: Root | null = null

describe('HookSettingsPanel', () => {
  beforeEach(() => {
    ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
      .IS_REACT_ACT_ENVIRONMENT = true
  })

  afterEach(async () => {
    await act(async () => root?.unmount())
    root = null
    document.body.replaceChildren()
    vi.restoreAllMocks()
    vi.mocked(fetchJson).mockReset()
  })

  it('shows hook status and the full-environment warning', async () => {
    await renderPanel(settings(false), vi.fn())

    expect(document.body.textContent).toContain('包括 API key')
    expect(document.body.textContent).toContain('protect-shell')
    expect(document.body.textContent).toContain('未信任')
    expect(document.body.textContent).toContain('已禁用')
    expect(document.body.textContent).toContain('Fail Open')
  })

  it('trusts the exact project configuration after confirmation', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true)
    vi.mocked(fetchJson).mockResolvedValue(settings(true))
    const onChanged = vi.fn().mockResolvedValue(undefined)
    await renderPanel(settings(false), onChanged)

    const button = [...document.querySelectorAll('button')].find((candidate) =>
      candidate.textContent?.includes('信任当前配置'),
    )
    await act(async () => button?.click())

    expect(window.confirm).toHaveBeenCalledWith(expect.stringContaining('完整环境'))
    expect(fetchJson).toHaveBeenCalledWith('/api/hooks/trust', { method: 'POST' })
    expect(onChanged).toHaveBeenCalledOnce()
  })

  it('does not trust project hooks when confirmation is declined', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(false)
    await renderPanel(settings(false), vi.fn())

    const button = [...document.querySelectorAll('button')].find((candidate) =>
      candidate.textContent?.includes('信任当前配置'),
    )
    await act(async () => button?.click())

    expect(fetchJson).not.toHaveBeenCalled()
  })
})

async function renderPanel(
  currentSettings: HookSettingsResponse,
  onChanged: () => Promise<void>,
) {
  const container = document.createElement('div')
  document.body.append(container)
  root = createRoot(container)
  await act(async () => {
    root?.render(
      <HookSettingsPanel settings={currentSettings} onChanged={onChanged} />,
    )
  })
}

function settings(trusted: boolean): HookSettingsResponse {
  return {
    schema_version: 1,
    user_config_path: '/home/test/.morrow/hooks.toml',
    project_config_path: '/workspace/.morrow/hooks.toml',
    trust_store_path: '/home/test/.morrow/hook-trust.json',
    project_fingerprint: 'a'.repeat(64),
    project_trusted: trusted,
    diagnostics: trusted ? [] : ['Project hooks are disabled until trusted.'],
    hooks: [
      {
        id: 'protect-shell',
        event: 'before_tool',
        command: ['./scripts/morrow-hook'],
        timeout_secs: 10,
        failure_mode: 'open',
        tool_names: ['shell_command'],
        agent_scopes: ['main'],
        source: 'project',
        trusted,
        active: trusted,
      },
    ],
  }
}
