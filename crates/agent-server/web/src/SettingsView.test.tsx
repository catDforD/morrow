// @vitest-environment jsdom

import { act } from 'react'
import { createRoot } from 'react-dom/client'
import type { Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import SettingsView from './SettingsView'
import type { StatusResponse } from './types'

let root: Root | null = null

describe('SettingsView diagnostics', () => {
  beforeEach(() => {
    ;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
      .IS_REACT_ACT_ENVIRONMENT = true
  })

  afterEach(async () => {
    await act(async () => root?.unmount())
    root = null
    document.body.replaceChildren()
    vi.clearAllMocks()
  })

  it('shows server configuration diagnostics on the about page', async () => {
    await renderAbout({
      ...status(),
      config_diagnostics: [
        'ignored /workspace/AGENTS.md: AGENTS.md is not valid UTF-8',
      ],
    })

    expect(document.body.textContent).toContain('配置诊断')
    expect(document.body.textContent).toContain('AGENTS.md is not valid UTF-8')
    expect(document.querySelector('.settings-diagnostics-note')).not.toBeNull()
  })

  it('omits the diagnostic card when startup has no diagnostics', async () => {
    await renderAbout(status())

    expect(document.querySelector('.settings-diagnostics-note')).toBeNull()
  })
})

async function renderAbout(currentStatus: StatusResponse) {
  const container = document.createElement('div')
  document.body.append(container)
  root = createRoot(container)
  await act(async () => {
    root?.render(
      <SettingsView
        section="about"
        status={currentStatus}
        theme="system"
        permissionMode="workspace_write"
        modelSettings={null}
        commandSettings={null}
        subagentSettings={null}
        isSidebarOpen={false}
        isSidebarHidden={false}
        onSectionChange={vi.fn()}
        onBack={vi.fn()}
        onOpenSidebar={vi.fn()}
        onCloseSidebar={vi.fn()}
        onThemeChange={vi.fn()}
        onPermissionModeChange={vi.fn()}
        onModelSettingsChange={vi.fn()}
        onCommandSettingsChange={vi.fn()}
        onSubagentSettingsChange={vi.fn()}
      />,
    )
  })
}

function status(): StatusResponse {
  return {
    workspace_root: '/workspace/morrow',
    workspace_location: { kind: 'local', path: '/workspace/morrow' },
    config_path: null,
    permissions: { mode: 'workspace_write', shell: 'prompt' },
    version: '0.3.1',
    model_ready: true,
    model_store_path: '/models.json',
    mcp_store_path: '/mcp.json',
    command_store_path: '/commands',
    subagent_store_path: '/subagents.json',
    config_diagnostics: [],
  }
}
