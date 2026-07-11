import { useEffect, useMemo, useState } from 'react'
import {
  Bot,
  Check,
  CircleAlert,
  Plus,
  RefreshCw,
  Save,
  Server,
  Sparkles,
  Trash2,
  Wrench,
} from 'lucide-react'
import { fetchJson } from './api'
import type {
  DiscoverModelsResponse,
  ManagedModel,
  ModelProviderResponse,
  ModelSelection,
  ModelSettingsResponse,
  ProviderWriteRequest,
  ReasoningLevel,
  ReasoningProfile,
} from './types'

type ProviderDraft = {
  id: string | null
  name: string
  base_url: string
  api_key: string
  api_key_configured: boolean
  enabled: boolean
  timeout_secs: number
  models: ManagedModel[]
  read_only: boolean
  make_default: boolean
  default_model_id: string
  default_reasoning: ReasoningLevel
}

const deepseekModels: ManagedModel[] = [
  {
    id: 'deepseek-v4-flash',
    name: 'DeepSeek V4 Flash',
    context_window_tokens: 1_000_000,
    reserved_output_tokens: 8_192,
    supports_tools: true,
    reasoning_profile: 'deepseek',
  },
  {
    id: 'deepseek-v4-pro',
    name: 'DeepSeek V4 Pro',
    context_window_tokens: 1_000_000,
    reserved_output_tokens: 8_192,
    supports_tools: true,
    reasoning_profile: 'deepseek',
  },
]

export default function ModelSettingsPanel({
  settings,
  onChanged,
}: {
  settings: ModelSettingsResponse | null
  onChanged: () => Promise<void>
}) {
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [draft, setDraft] = useState<ProviderDraft | null>(null)
  const [saving, setSaving] = useState(false)
  const [discovering, setDiscovering] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const selectedProvider = useMemo(
    () => settings?.providers.find((provider) => provider.id === selectedId),
    [selectedId, settings],
  )

  useEffect(() => {
    if (draft && draft.id === null) return
    const provider =
      selectedProvider ?? settings?.providers[0] ?? null
    if (!provider) {
      setSelectedId(null)
      setDraft(null)
      return
    }
    setSelectedId(provider.id)
    setDraft(providerDraft(provider, settings?.default_selection ?? null))
  }, [selectedProvider, settings])

  const selectProvider = (provider: ModelProviderResponse) => {
    setSelectedId(provider.id)
    setDraft(providerDraft(provider, settings?.default_selection ?? null))
    setError(null)
  }

  const startProvider = (template: 'deepseek' | 'custom') => {
    const models = template === 'deepseek' ? deepseekModels.map(copyModel) : []
    setSelectedId(null)
    setDraft({
      id: null,
      name: template === 'deepseek' ? 'DeepSeek' : '',
      base_url:
        template === 'deepseek' ? 'https://api.deepseek.com' : 'https://api.example.com/v1',
      api_key: '',
      api_key_configured: false,
      enabled: true,
      timeout_secs: 120,
      models,
      read_only: false,
      make_default: !settings?.default_selection,
      default_model_id: '',
      default_reasoning: 'high',
    })
    setError(null)
  }

  const saveProvider = async () => {
    if (!draft || draft.read_only) return
    const validation = validateDraft(draft, settings?.default_selection ?? null)
    if (validation) {
      setError(validation)
      return
    }
    setSaving(true)
    setError(null)
    try {
      const request: ProviderWriteRequest = {
        name: draft.name.trim(),
        base_url: draft.base_url.trim(),
        enabled: draft.enabled,
        timeout_secs: draft.timeout_secs,
        models: draft.models.map(normalizeModel),
      }
      if (draft.api_key.trim()) request.api_key = draft.api_key.trim()
      if (draft.make_default) {
        request.default_model = {
          model_id: draft.default_model_id.trim(),
          reasoning: reasoningForModel(
            draft.models,
            draft.default_model_id,
            draft.default_reasoning,
          ),
        }
      }
      const url = draft.id
        ? `/api/model-providers/${encodeURIComponent(draft.id)}`
        : '/api/model-providers'
      const provider = await fetchJson<ModelProviderResponse>(url, {
        method: draft.id ? 'PUT' : 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(request),
      })
      setSelectedId(provider.id)
      setDraft(null)
      await onChanged()
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  const deleteProvider = async () => {
    if (!draft?.id || draft.read_only) return
    if (!window.confirm(`删除供应商“${draft.name}”？已保存的 API Key 也会删除。`)) {
      return
    }
    setSaving(true)
    setError(null)
    try {
      await fetchJson<unknown>(
        `/api/model-providers/${encodeURIComponent(draft.id)}`,
        { method: 'DELETE' },
      ).catch((caught) => {
        if (caught instanceof SyntaxError) return undefined
        throw caught
      })
      setSelectedId(null)
      setDraft(null)
      await onChanged()
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  const discoverModels = async () => {
    if (!draft) return
    if (!draft.id && (!draft.base_url.trim() || !draft.api_key.trim())) {
      setError('新供应商同步模型前需要填写 Base URL 和 API Key。')
      return
    }
    setDiscovering(true)
    setError(null)
    try {
      const response = await fetchJson<DiscoverModelsResponse>(
        '/api/model-providers/discover',
        {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(
            draft.id
              ? { provider_id: draft.id }
              : {
                  base_url: draft.base_url.trim(),
                  api_key: draft.api_key.trim(),
                  timeout_secs: draft.timeout_secs,
                },
          ),
        },
      )
      const existing = new Set(draft.models.map((model) => model.id))
      const discovered = response.models
        .filter((model) => !existing.has(model.id))
        .map(
          (model): ManagedModel =>
            model.suggested ?? {
              id: model.id,
              name: model.id,
              context_window_tokens: 0,
              reserved_output_tokens: 8_192,
              supports_tools: false,
              reasoning_profile: 'none',
            },
        )
      setDraft({ ...draft, models: [...draft.models, ...discovered] })
      if (discovered.length === 0) {
        setError('远端没有返回新的模型；现有本地配置保持不变。')
      }
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setDiscovering(false)
    }
  }

  const setDefault = async (selection: ModelSelection) => {
    setSaving(true)
    setError(null)
    try {
      await fetchJson<ModelSelection>('/api/model-default', {
        method: 'PUT',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(selection),
      })
      await onChanged()
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="settings-page model-settings-page" aria-labelledby="model-settings-title">
      <header className="settings-page-header model-settings-header">
        <div>
          <p className="eyebrow">Settings</p>
          <h1 id="model-settings-title">模型设置</h1>
          <p>管理 Web 专用的 OpenAI-compatible 供应商，并选择聊天默认模型。</p>
        </div>
        <button
          className="secondary-button"
          type="button"
          disabled={discovering || !draft || draft.read_only}
          onClick={() => void discoverModels()}
        >
          <RefreshCw size={16} className={discovering ? 'spinning' : undefined} />
          <span>{discovering ? '同步中' : '同步模型'}</span>
        </button>
      </header>

      <div className="model-settings-shell">
        <aside className="model-provider-list" aria-label="模型供应商">
          <div className="model-provider-list-heading">
            <strong>供应商</strong>
            <span>{settings?.providers.length ?? 0}</span>
          </div>
          <div className="model-provider-scroll">
            {settings?.providers.map((provider) => (
              <button
                className={`model-provider-item${provider.id === selectedId ? ' active' : ''}`}
                type="button"
                key={provider.id}
                onClick={() => selectProvider(provider)}
              >
                <span className="model-provider-mark">
                  {provider.id === 'current-config' ? <Server size={17} /> : <Bot size={17} />}
                </span>
                <span>
                  <strong>{provider.name}</strong>
                  <small>{provider.models.length} 个模型</small>
                </span>
                <i className={provider.enabled ? 'ready' : 'disabled'} />
              </button>
            ))}
          </div>
          <div className="model-provider-add-actions">
            <button type="button" onClick={() => startProvider('deepseek')}>
              <Sparkles size={16} />
              <span>添加 DeepSeek</span>
            </button>
            <button type="button" onClick={() => startProvider('custom')}>
              <Plus size={16} />
              <span>自定义供应商</span>
            </button>
          </div>
        </aside>

        <div className="model-provider-editor">
          {draft ? (
            <ProviderEditor
              draft={draft}
              settings={settings}
              saving={saving}
              onChange={setDraft}
              onSave={() => void saveProvider()}
              onDelete={() => void deleteProvider()}
              onSetDefault={(selection) => void setDefault(selection)}
            />
          ) : (
            <div className="model-settings-empty">
              <Bot size={30} />
              <h2>添加第一个模型供应商</h2>
              <p>可以使用 DeepSeek V4 模板，或配置任意 OpenAI-compatible API。</p>
              <button type="button" onClick={() => startProvider('deepseek')}>
                <Sparkles size={16} />
                使用 DeepSeek 模板
              </button>
            </div>
          )}
          {error ? (
            <div className="model-settings-error" role="alert">
              <CircleAlert size={17} />
              <span>{error}</span>
            </div>
          ) : null}
        </div>
      </div>

    </section>
  )
}

function ProviderEditor({
  draft,
  settings,
  saving,
  onChange,
  onSave,
  onDelete,
  onSetDefault,
}: {
  draft: ProviderDraft
  settings: ModelSettingsResponse | null
  saving: boolean
  onChange: (draft: ProviderDraft) => void
  onSave: () => void
  onDelete: () => void
  onSetDefault: (selection: ModelSelection) => void
}) {
  const updateModel = (index: number, model: ManagedModel) => {
    const current = draft.models[index]
    const models = draft.models.map((current, currentIndex) =>
      currentIndex === index ? model : current,
    )
    const default_model_id =
      draft.default_model_id === current?.id ? model.id : draft.default_model_id
    onChange({ ...draft, models, default_model_id })
  }

  const removeModel = (index: number) => {
    const models = draft.models.filter((_, currentIndex) => currentIndex !== index)
    const default_model_id =
      draft.default_model_id === draft.models[index]?.id ? '' : draft.default_model_id
    onChange({ ...draft, models, default_model_id })
  }

  const addModel = () => {
    onChange({
      ...draft,
      models: [
        ...draft.models,
        {
          id: '',
          name: '',
          context_window_tokens: 128_000,
          reserved_output_tokens: 8_192,
          supports_tools: false,
          reasoning_profile: 'none',
        },
      ],
    })
  }

  if (draft.read_only) {
    const defaultSelection = settings?.default_selection
    const model = draft.models[0]
    return (
      <div className="readonly-provider">
        <div className="provider-editor-title">
          <div>
            <p className="eyebrow">Runtime config</p>
            <h2>{draft.name}</h2>
          </div>
          <span className="enabled-badge"><Check size={14} /> 已启用</span>
        </div>
        <p>该供应商来自当前 morrow.toml，只读且不会把 API Key 暴露给浏览器。</p>
        <dl className="settings-card settings-info-list">
          <div className="settings-info-row"><dt>Base URL</dt><dd>{draft.base_url}</dd></div>
          <div className="settings-info-row"><dt>模型</dt><dd>{model?.name ?? '—'}</dd></div>
          <div className="settings-info-row"><dt>上下文</dt><dd>{model ? compactTokens(model.context_window_tokens) : '—'}</dd></div>
          <div className="settings-info-row"><dt>全局默认</dt><dd>{defaultSelection?.provider_id === draft.id ? '是' : '否'}</dd></div>
        </dl>
        {defaultSelection?.provider_id !== draft.id && model ? (
          <button
            className="approve-button"
            type="button"
            disabled={saving}
            onClick={() =>
              onSetDefault({
                provider_id: draft.id ?? 'current-config',
                model_id: model.id,
                reasoning:
                  model.reasoning_profile === 'deepseek' ? 'high' : 'off',
              })
            }
          >
            <Check size={15} /> 设为全局默认
          </button>
        ) : null}
      </div>
    )
  }

  return (
    <form
      className="provider-form"
      onSubmit={(event) => {
        event.preventDefault()
        onSave()
      }}
    >
      <div className="provider-editor-title">
        <div>
          <p className="eyebrow">OpenAI-compatible</p>
          <h2>{draft.id ? draft.name || '编辑供应商' : '添加模型供应商'}</h2>
        </div>
        <label className="provider-enabled-toggle">
          <input
            type="checkbox"
            checked={draft.enabled}
            onChange={(event) => onChange({ ...draft, enabled: event.target.checked })}
          />
          <span>{draft.enabled ? '已启用' : '已停用'}</span>
        </label>
      </div>

      <div className="provider-field-grid">
        <label>
          <span>名称</span>
          <input
            value={draft.name}
            placeholder="例如：DeepSeek"
            onChange={(event) => onChange({ ...draft, name: event.target.value })}
          />
        </label>
        <label>
          <span>API 格式</span>
          <input value="OpenAI Chat Completions" disabled />
        </label>
        <label className="full">
          <span>Base URL</span>
          <input
            value={draft.base_url}
            placeholder="https://api.example.com/v1"
            onChange={(event) => onChange({ ...draft, base_url: event.target.value })}
          />
        </label>
        <label className="full">
          <span>API Key</span>
          <input
            value={draft.api_key}
            type="password"
            autoComplete="off"
            placeholder={draft.api_key_configured ? '留空以保留已保存 Key' : '输入 API Key'}
            onChange={(event) => onChange({ ...draft, api_key: event.target.value })}
          />
        </label>
      </div>

      <details className="provider-advanced">
        <summary>供应商高级设置</summary>
        <label>
          <span>请求超时（秒）</span>
          <input
            type="number"
            min={1}
            max={600}
            value={draft.timeout_secs}
            onChange={(event) =>
              onChange({ ...draft, timeout_secs: Number(event.target.value) })
            }
          />
        </label>
      </details>

      <section className="provider-models-section">
        <div className="provider-models-heading">
          <div>
            <strong>模型列表</strong>
            <p>上下文参数用于 Morrow 自动压缩预算。</p>
          </div>
          <button type="button" onClick={addModel}>
            <Plus size={15} /> 添加模型
          </button>
        </div>
        <div className="provider-model-list">
          {draft.models.map((model, index) => (
            <ModelEditor
              key={`${index}-${model.id}`}
              model={model}
              onChange={(next) => updateModel(index, next)}
              onDelete={() => removeModel(index)}
            />
          ))}
          {draft.models.length === 0 ? (
            <button className="empty-model-row" type="button" onClick={addModel}>
              <Plus size={16} /> 添加首个模型
            </button>
          ) : null}
        </div>
      </section>

      <section className="provider-default-section">
        <label className="default-checkbox">
          <input
            type="checkbox"
            checked={draft.make_default}
            disabled={settings?.default_selection?.provider_id === draft.id}
            onChange={(event) => onChange({ ...draft, make_default: event.target.checked })}
          />
          <span>设为全局默认模型</span>
        </label>
        {draft.make_default ? (
          <div className="provider-default-controls">
            <select
              value={draft.default_model_id}
              onChange={(event) => {
                const model = draft.models.find((item) => item.id === event.target.value)
                onChange({
                  ...draft,
                  default_model_id: event.target.value,
                  default_reasoning:
                    model?.reasoning_profile === 'deepseek' ? 'high' : 'off',
                })
              }}
            >
              <option value="">选择默认模型</option>
              {draft.models.filter((model) => model.id.trim()).map((model) => (
                <option value={model.id} key={model.id}>{model.name || model.id}</option>
              ))}
            </select>
            {draft.models.find((model) => model.id === draft.default_model_id)
              ?.reasoning_profile === 'deepseek' ? (
              <select
                value={draft.default_reasoning}
                onChange={(event) =>
                  onChange({
                    ...draft,
                    default_reasoning: event.target.value as ReasoningLevel,
                  })
                }
              >
                <option value="off">关闭思考</option>
                <option value="high">高</option>
                <option value="max">最高</option>
              </select>
            ) : null}
          </div>
        ) : null}
      </section>

      <div className="provider-form-actions">
        {draft.id ? (
          <button className="danger-button subtle" type="button" disabled={saving} onClick={onDelete}>
            <Trash2 size={15} /> 删除供应商
          </button>
        ) : <span />}
        <button className="approve-button" type="submit" disabled={saving}>
          <Save size={16} /> {saving ? '保存中…' : '保存供应商'}
        </button>
      </div>
    </form>
  )
}

function ModelEditor({
  model,
  onChange,
  onDelete,
}: {
  model: ManagedModel
  onChange: (model: ManagedModel) => void
  onDelete: () => void
}) {
  return (
    <article className={`provider-model-row${model.context_window_tokens <= 0 ? ' incomplete' : ''}`}>
      <div className="model-row-primary">
        <Bot size={17} />
        <input
          aria-label="模型显示名称"
          value={model.name}
          placeholder="显示名称"
          onChange={(event) => onChange({ ...model, name: event.target.value })}
        />
        <input
          aria-label="模型 ID"
          value={model.id}
          placeholder="model-id"
          onChange={(event) => onChange({ ...model, id: event.target.value })}
        />
        <span>{model.context_window_tokens > 0 ? compactTokens(model.context_window_tokens) : '待配置'}</span>
        <button type="button" title="删除模型" onClick={onDelete}><Trash2 size={15} /></button>
      </div>
      <details>
        <summary>高级能力</summary>
        <div className="model-row-advanced">
          <label>
            <span>上下文 token</span>
            <input
              type="number"
              min={1}
              value={model.context_window_tokens}
              onChange={(event) =>
                onChange({ ...model, context_window_tokens: Number(event.target.value) })
              }
            />
          </label>
          <label>
            <span>预留输出 token</span>
            <input
              type="number"
              min={1}
              value={model.reserved_output_tokens}
              onChange={(event) =>
                onChange({ ...model, reserved_output_tokens: Number(event.target.value) })
              }
            />
          </label>
          <label>
            <span>思考协议</span>
            <select
              value={model.reasoning_profile}
              onChange={(event) =>
                onChange({ ...model, reasoning_profile: event.target.value as ReasoningProfile })
              }
            >
              <option value="none">不支持</option>
              <option value="deepseek">DeepSeek thinking</option>
            </select>
          </label>
          <label className="model-tool-toggle">
            <input
              type="checkbox"
              checked={model.supports_tools}
              onChange={(event) => onChange({ ...model, supports_tools: event.target.checked })}
            />
            <Wrench size={15} /> 支持工具调用
          </label>
        </div>
      </details>
    </article>
  )
}

function providerDraft(
  provider: ModelProviderResponse,
  defaultSelection: ModelSelection | null,
): ProviderDraft {
  const isDefault = defaultSelection?.provider_id === provider.id
  return {
    id: provider.id,
    name: provider.name,
    base_url: provider.base_url,
    api_key: '',
    api_key_configured: provider.api_key_configured,
    enabled: provider.enabled,
    timeout_secs: provider.timeout_secs,
    models: provider.models.map(copyModel),
    read_only: provider.read_only,
    make_default: isDefault,
    default_model_id: isDefault ? defaultSelection.model_id : '',
    default_reasoning: isDefault ? defaultSelection.reasoning : 'high',
  }
}

function validateDraft(
  draft: ProviderDraft,
  currentDefault: ModelSelection | null,
): string | null {
  if (!draft.name.trim()) return '供应商名称不能为空。'
  if (!/^https?:\/\//.test(draft.base_url.trim())) return 'Base URL 必须以 http:// 或 https:// 开头。'
  if (!draft.api_key_configured && !draft.api_key.trim()) return 'API Key 不能为空。'
  if (draft.timeout_secs < 1 || draft.timeout_secs > 600) return '请求超时必须在 1 到 600 秒之间。'
  if (draft.models.length === 0) return '至少添加一个模型。'
  const ids = new Set<string>()
  for (const model of draft.models) {
    if (!model.id.trim() || !model.name.trim()) return '每个模型都需要 ID 和显示名称。'
    if (ids.has(model.id.trim())) return `模型 ID ${model.id} 重复。`
    ids.add(model.id.trim())
    if (
      model.context_window_tokens <= 0 ||
      model.reserved_output_tokens <= 0 ||
      model.reserved_output_tokens >= model.context_window_tokens
    ) {
      return `模型 ${model.name || model.id} 的上下文参数无效。`
    }
  }
  if (!currentDefault && !draft.make_default) return '首次配置必须明确选择全局默认模型。'
  if (draft.make_default && !draft.default_model_id) return '请选择全局默认模型。'
  return null
}

function reasoningForModel(
  models: ManagedModel[],
  modelId: string,
  reasoning: ReasoningLevel,
): ReasoningLevel {
  return models.find((model) => model.id === modelId)?.reasoning_profile === 'deepseek'
    ? reasoning
    : 'off'
}

function normalizeModel(model: ManagedModel): ManagedModel {
  return {
    ...model,
    id: model.id.trim(),
    name: model.name.trim(),
  }
}

function copyModel(model: ManagedModel): ManagedModel {
  return { ...model }
}

function compactTokens(tokens: number): string {
  if (tokens >= 1_000_000) return `${tokens / 1_000_000}M`
  if (tokens >= 1_000) return `${Math.round(tokens / 1_000)}K`
  return String(tokens)
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
