// @vitest-environment jsdom

import { describe, expect, it } from 'vitest'
import { completeRunningModelStep, modelStepPresentation } from './App'
import type {
  ModelSelection,
  ModelSettingsResponse,
  RunTrace,
} from './types'

const selection: ModelSelection = {
  provider_id: 'opencode',
  model_id: 'deepseek-v4-pro',
  reasoning: 'max',
}

const settings: ModelSettingsResponse = {
  providers: [
    {
      id: 'opencode',
      name: 'opencode',
      base_url: 'https://models.example/v1',
      api_format: 'openai_chat_completions',
      enabled: true,
      read_only: false,
      api_key_configured: true,
      timeout_secs: 120,
      models: [
        {
          id: 'deepseek-v4-pro',
          name: 'DeepSeek V4 Pro',
          context_window_tokens: 1_000_000,
          reserved_output_tokens: 32_000,
          supports_tools: true,
          reasoning_profile: 'deepseek',
        },
      ],
    },
  ],
  default_selection: selection,
  model_ready: true,
  store_path: '/tmp/models.json',
}

describe('modelStepPresentation', () => {
  it('uses the selected model name for live model steps', () => {
    expect(modelStepPresentation(settings, selection)).toEqual({
      title: 'DeepSeek V4 Pro',
      detail: 'opencode · 最高',
    })
  })

  it('falls back only when the model cannot be resolved', () => {
    expect(modelStepPresentation(settings, null)).toEqual({
      title: 'Model call',
    })
  })
})

describe('completeRunningModelStep', () => {
  it('does not create duplicate model rows for one concurrent tool batch', () => {
    const trace: RunTrace = {
      id: 'run-1',
      status: 'running',
      collapsed: false,
      startedAt: 'now',
      toolCount: 0,
      steps: [
        {
          id: 'model-1',
          kind: 'model',
          status: 'running',
          title: 'DeepSeek V4 Pro',
        },
      ],
    }

    const firstSubagent = completeRunningModelStep(trace)
    const fourthSubagent = completeRunningModelStep(
      completeRunningModelStep(
        completeRunningModelStep(firstSubagent),
      ),
    )

    expect(fourthSubagent.steps).toHaveLength(1)
    expect(fourthSubagent.steps[0]).toMatchObject({
      id: 'model-1',
      kind: 'model',
      status: 'ok',
    })
  })
})
