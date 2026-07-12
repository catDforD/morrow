import { useMemo, useState } from 'react'
import {
  ArrowLeft,
  CircleAlert,
  FileText,
  Plus,
  Save,
  Search,
  Terminal,
  Trash2,
  X,
} from 'lucide-react'
import { fetchJson } from './api'
import type {
  CommandDefinition,
  CommandSettingsResponse,
  CommandWriteRequest,
} from './types'

type CommandDraft = CommandWriteRequest & {
  originalName: string | null
}

export default function CommandSettingsPanel({
  settings,
  onChanged,
}: {
  settings: CommandSettingsResponse | null
  onChanged: () => Promise<void>
}) {
  const [query, setQuery] = useState('')
  const [draft, setDraft] = useState<CommandDraft | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const commands = useMemo(() => {
    const normalized = query.trim().toLowerCase()
    if (!normalized) return settings?.commands ?? []
    return (settings?.commands ?? []).filter((command) =>
      `${command.name} ${command.description}`.toLowerCase().includes(normalized),
    )
  }, [query, settings])

  const editCommand = (command: CommandDefinition) => {
    setDraft({ ...command, originalName: command.name })
    setError(null)
  }

  const createCommand = () => {
    setDraft({
      originalName: null,
      name: '',
      description: '',
      argument_hint: '',
      prompt: '',
    })
    setError(null)
  }

  const saveCommand = async () => {
    if (!draft) return
    const validation = validateDraft(draft)
    if (validation) {
      setError(validation)
      return
    }
    setSaving(true)
    setError(null)
    try {
      const request: CommandWriteRequest = {
        name: draft.name.trim(),
        description: draft.description.trim(),
        argument_hint: draft.argument_hint.trim(),
        prompt: draft.prompt.trim(),
      }
      const url = draft.originalName
        ? `/api/commands/${encodeURIComponent(draft.originalName)}`
        : '/api/commands'
      await fetchJson<CommandDefinition>(url, {
        method: draft.originalName ? 'PUT' : 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(request),
      })
      await onChanged()
      setDraft(null)
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  const deleteCommand = async () => {
    if (!draft?.originalName) return
    if (!window.confirm(`删除命令“/${draft.name}”？`)) return
    setSaving(true)
    setError(null)
    try {
      await fetchJson<unknown>(
        `/api/commands/${encodeURIComponent(draft.originalName)}`,
        { method: 'DELETE' },
      )
      await onChanged()
      setDraft(null)
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="settings-page resource-settings-page" aria-labelledby="command-settings-title">
      <header className="settings-page-header resource-settings-header">
        <div>
          <p className="eyebrow">Settings</p>
          <h1 id="command-settings-title">命令</h1>
          <p>管理 Web 聊天可通过 /command-name 调用的 Markdown 提示词。</p>
        </div>
        {!draft ? (
          <button className="approve-button" type="button" onClick={createCommand}>
            <Plus size={16} /> 新建命令
          </button>
        ) : null}
      </header>

      {draft ? (
        <div className="resource-editor-view">
          <button className="resource-back-link" type="button" onClick={() => setDraft(null)}>
            <ArrowLeft size={16} /> 返回命令列表
          </button>
          <form className="resource-form-card command-form" onSubmit={(event) => { event.preventDefault(); void saveCommand() }}>
            <div className="resource-form-heading">
              <div>
                <p className="eyebrow">Markdown command</p>
                <h2>{draft.originalName ? `编辑 /${draft.originalName}` : '新建命令'}</h2>
                <p>提示词正文中的 $ARGUMENTS 会替换为调用时输入的全部参数。</p>
              </div>
              <span className="scope-badge">用户</span>
            </div>
            <div className="resource-field-grid">
              <label className="resource-field">
                <span>名称</span>
                <div className="command-name-input"><Terminal size={16} /><span>/</span><input value={draft.name} placeholder="my-command" onChange={(event) => setDraft({ ...draft, name: event.target.value })} /></div>
              </label>
              <label className="resource-field">
                <span>参数提示（可选）</span>
                <input value={draft.argument_hint} placeholder="例如：<file-path>" onChange={(event) => setDraft({ ...draft, argument_hint: event.target.value })} />
              </label>
              <label className="resource-field full">
                <span>描述（可选）</span>
                <input value={draft.description} placeholder="在命令建议中显示的简短描述" onChange={(event) => setDraft({ ...draft, description: event.target.value })} />
              </label>
              <label className="resource-field full">
                <span>提示词</span>
                <textarea className="command-prompt-editor" value={draft.prompt} placeholder="填写调用该命令时发送给模型的提示词…" onChange={(event) => setDraft({ ...draft, prompt: event.target.value })} />
              </label>
            </div>
            <div className="command-template-help">
              <FileText size={17} />
              <span>保存为 <code>~/.morrow/commands/{draft.name || 'command'}.md</code>。模板没有 $ARGUMENTS 时，参数会自动附加到正文末尾。</span>
            </div>
            <div className="resource-form-actions split">
              {draft.originalName ? (
                <button className="danger-button subtle" type="button" disabled={saving} onClick={() => void deleteCommand()}>
                  <Trash2 size={15} /> 删除命令
                </button>
              ) : <span />}
              <button className="approve-button" type="submit" disabled={saving}>
                <Save size={16} /> {saving ? '保存中…' : '保存命令'}
              </button>
            </div>
          </form>
          {error ? <ResourceError message={error} /> : null}
        </div>
      ) : (
        <div className="resource-list-view">
          <label className="resource-search">
            <Search size={17} />
            <input value={query} placeholder="搜索命令…" onChange={(event) => setQuery(event.target.value)} />
            {query ? <button type="button" title="清除搜索" onClick={() => setQuery('')}><X size={15} /></button> : null}
          </label>
          <div className="resource-list-heading">
            <strong>用户命令</strong>
            <span>{commands.length} 项</span>
          </div>
          <div className="resource-list-card">
            {commands.length === 0 ? (
              <div className="resource-empty">
                <Terminal size={28} />
                <strong>{query ? '没有匹配的命令' : '尚未创建命令'}</strong>
                <span>创建 Markdown 提示词，然后在聊天框中输入 / 调用。</span>
              </div>
            ) : null}
            {commands.map((command) => (
              <button className="resource-list-row" type="button" key={command.name} onClick={() => editCommand(command)}>
                <span className="resource-list-icon"><Terminal size={18} /></span>
                <span className="resource-list-copy">
                  <span><strong>/{command.name}</strong>{command.argument_hint ? <small>{command.argument_hint}</small> : null}<small>用户</small></span>
                  <small>{command.description || compactPrompt(command.prompt)}</small>
                </span>
              </button>
            ))}
          </div>
          {settings?.diagnostics.length ? (
            <div className="resource-diagnostics" role="status">
              <CircleAlert size={17} />
              <span>{settings.diagnostics.join('\n')}</span>
            </div>
          ) : null}
          {error ? <ResourceError message={error} /> : null}
        </div>
      )}
    </section>
  )
}

function ResourceError({ message }: { message: string }) {
  return <div className="model-settings-error resource-error" role="alert"><CircleAlert size={17} /><span>{message}</span></div>
}

function validateDraft(draft: CommandDraft): string | null {
  if (!/^[a-z0-9][a-z0-9_-]{0,63}$/.test(draft.name.trim())) return '名称只能包含小写字母、数字、- 和 _，长度不超过 64。'
  if (draft.description.trim().length > 200) return '描述不能超过 200 个字符。'
  if (draft.argument_hint.trim().length > 120) return '参数提示不能超过 120 个字符。'
  if (!draft.prompt.trim()) return '提示词不能为空。'
  return null
}

function compactPrompt(prompt: string): string {
  const compact = prompt.replace(/\s+/g, ' ').trim()
  return compact.length > 110 ? `${compact.slice(0, 107)}…` : compact
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
