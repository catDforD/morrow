import { useEffect, useMemo, useState } from 'react'
import {
  ArrowLeft,
  Braces,
  Check,
  CircleAlert,
  Globe2,
  Plus,
  RefreshCw,
  Save,
  Search,
  Server,
  Terminal,
  Trash2,
  X,
} from 'lucide-react'
import { fetchJson } from './api'
import type {
  McpInspection,
  McpServerResponse,
  McpServerWriteRequest,
  McpSettingsResponse,
  McpTransport,
} from './types'

type SecretRow = {
  key: string
  value: string
  originalKey?: string
}

type McpDraft = {
  originalName: string | null
  name: string
  transport: McpTransport
  command: string
  argsText: string
  env: SecretRow[]
  cwd: string
  url: string
  headers: SecretRow[]
  enabled: boolean
  startupTimeout: number
  toolTimeout: number
}

const jsonTemplate = `{
  "mcpServers": {
    "my-mcp-server": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-memory"],
      "env": {},
      "enabled": true,
      "startup_timeout_sec": 10,
      "tool_timeout_sec": 60
    }
  }
}`

export default function McpSettingsPanel() {
  const [settings, setSettings] = useState<McpSettingsResponse | null>(null)
  const [query, setQuery] = useState('')
  const [draft, setDraft] = useState<McpDraft | null>(null)
  const [readOnly, setReadOnly] = useState<McpServerResponse | null>(null)
  const [jsonMode, setJsonMode] = useState(false)
  const [jsonValue, setJsonValue] = useState(jsonTemplate)
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [testing, setTesting] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [inspection, setInspection] = useState<McpInspection | null>(null)

  const loadSettings = async () => {
    const response = await fetchJson<McpSettingsResponse>('/api/mcp-settings')
    setSettings(response)
    return response
  }

  useEffect(() => {
    let active = true
    void loadSettings()
      .catch((caught) => {
        if (active) setError(errorMessage(caught))
      })
      .finally(() => {
        if (active) setLoading(false)
      })
    return () => {
      active = false
    }
  }, [])

  const filteredServers = useMemo(() => {
    const normalized = query.trim().toLowerCase()
    if (!normalized) return settings?.servers ?? []
    return (settings?.servers ?? []).filter((server) =>
      `${server.name} ${server.transport} ${server.source}`
        .toLowerCase()
        .includes(normalized),
    )
  }, [query, settings])

  const openServer = (server: McpServerResponse) => {
    setError(null)
    setInspection(null)
    setJsonMode(false)
    if (server.read_only) {
      setDraft(null)
      setReadOnly(server)
      return
    }
    setReadOnly(null)
    setDraft(draftFromServer(server))
  }

  const startCreate = (mode: 'form' | 'json') => {
    setReadOnly(null)
    setError(null)
    setInspection(null)
    setJsonMode(mode === 'json')
    setJsonValue(jsonTemplate)
    setDraft(mode === 'form' ? emptyDraft() : null)
  }

  const backToList = () => {
    setDraft(null)
    setReadOnly(null)
    setJsonMode(false)
    setError(null)
    setInspection(null)
  }

  const saveDraft = async () => {
    if (!draft) return
    const validation = validateDraft(draft)
    if (validation) {
      setError(validation)
      return
    }
    setSaving(true)
    setError(null)
    try {
      const url = draft.originalName
        ? `/api/mcp-servers/${encodeURIComponent(draft.originalName)}`
        : '/api/mcp-servers'
      await fetchJson<McpServerResponse>(url, {
        method: draft.originalName ? 'PUT' : 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(requestFromDraft(draft)),
      })
      await loadSettings()
      backToList()
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  const importJson = async () => {
    setSaving(true)
    setError(null)
    try {
      const value = JSON.parse(jsonValue) as unknown
      await fetchJson<McpServerResponse[]>('/api/mcp-servers/import', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(value),
      })
      await loadSettings()
      backToList()
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  const testDraft = async () => {
    if (!draft) return
    const validation = validateDraft(draft)
    if (validation) {
      setError(validation)
      return
    }
    setTesting(true)
    setError(null)
    setInspection(null)
    try {
      const result = await fetchJson<McpInspection>('/api/mcp-servers/test', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          existing_name: draft.originalName,
          server: requestFromDraft(draft),
        }),
      })
      setInspection(result)
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setTesting(false)
    }
  }

  const deleteDraft = async () => {
    if (!draft?.originalName) return
    if (!window.confirm(`删除 MCP 服务器“${draft.name}”？`)) return
    setSaving(true)
    setError(null)
    try {
      await fetchJson<unknown>(
        `/api/mcp-servers/${encodeURIComponent(draft.originalName)}`,
        { method: 'DELETE' },
      )
      await loadSettings()
      backToList()
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  const editing = draft || readOnly || jsonMode

  return (
    <section className="settings-page resource-settings-page" aria-labelledby="mcp-settings-title">
      <header className="settings-page-header resource-settings-header">
        <div>
          <p className="eyebrow">Settings</p>
          <h1 id="mcp-settings-title">MCP 服务器</h1>
          <p>管理 Web Agent 使用的本地与远端 MCP 工具服务器。</p>
        </div>
        {!editing ? (
          <div className="resource-header-actions">
            <button className="secondary-button" type="button" onClick={() => startCreate('json')}>
              <Braces size={16} /> JSON 导入
            </button>
            <button className="approve-button" type="button" onClick={() => startCreate('form')}>
              <Plus size={16} /> 新建服务器
            </button>
          </div>
        ) : null}
      </header>

      {editing ? (
        <div className="resource-editor-view">
          <button className="resource-back-link" type="button" onClick={backToList}>
            <ArrowLeft size={16} /> 返回 MCP 列表
          </button>
          {readOnly ? <ReadOnlyServer server={readOnly} /> : null}
          {draft ? (
            <McpEditor
              draft={draft}
              saving={saving}
              testing={testing}
              inspection={inspection}
              onChange={setDraft}
              onSave={() => void saveDraft()}
              onTest={() => void testDraft()}
              onDelete={() => void deleteDraft()}
            />
          ) : null}
          {jsonMode ? (
            <div className="resource-form-card">
              <div className="resource-form-heading">
                <div>
                  <p className="eyebrow">JSON import</p>
                  <h2>导入 MCP 服务器</h2>
                  <p>支持直接服务器对象或 mcpServers 包装，可一次导入多项。</p>
                </div>
                <span className="scope-badge">用户</span>
              </div>
              <label className="resource-field full">
                <span>完整配置</span>
                <textarea
                  className="resource-code-editor"
                  value={jsonValue}
                  spellCheck={false}
                  onChange={(event) => setJsonValue(event.target.value)}
                />
              </label>
              <p className="resource-form-note">导入是原子操作；任意名称冲突或配置错误都会取消整批写入。</p>
              <div className="resource-form-actions">
                <button className="secondary-button" type="button" onClick={backToList}>取消</button>
                <button className="approve-button" type="button" disabled={saving} onClick={() => void importJson()}>
                  <Save size={16} /> {saving ? '导入中…' : '导入配置'}
                </button>
              </div>
            </div>
          ) : null}
          {error ? <ResourceError message={error} /> : null}
        </div>
      ) : (
        <div className="resource-list-view">
          <label className="resource-search">
            <Search size={17} />
            <input value={query} placeholder="搜索 MCP 服务器…" onChange={(event) => setQuery(event.target.value)} />
            {query ? <button type="button" title="清除搜索" onClick={() => setQuery('')}><X size={15} /></button> : null}
          </label>
          <div className="resource-list-heading">
            <strong>服务器</strong>
            <span>{filteredServers.length} 项</span>
          </div>
          <div className="resource-list-card">
            {loading ? <div className="resource-empty">正在加载 MCP 配置…</div> : null}
            {!loading && filteredServers.length === 0 ? (
              <div className="resource-empty">
                <Server size={28} />
                <strong>{query ? '没有匹配的服务器' : '尚未配置 MCP 服务器'}</strong>
                <span>可以通过表单新建，或粘贴标准 JSON 配置。</span>
              </div>
            ) : null}
            {filteredServers.map((server) => (
              <button className="resource-list-row" type="button" key={`${server.source}-${server.name}`} onClick={() => openServer(server)}>
                <span className="resource-list-icon">
                  {server.transport === 'stdio' ? <Terminal size={18} /> : <Globe2 size={18} />}
                </span>
                <span className="resource-list-copy">
                  <span><strong>{server.name}</strong><small>{server.transport}</small>{server.read_only ? <small>morrow.toml</small> : <small>用户</small>}</span>
                  <small>{server.read_only ? '运行时配置，只读' : server.enabled ? '下一次 turn 将加载此服务器' : '当前已停用'}</small>
                </span>
                <span className={`resource-status ${server.enabled ? 'ready' : 'disabled'}`}>
                  {server.enabled ? <Check size={13} /> : null}{server.enabled ? '已启用' : '已停用'}
                </span>
              </button>
            ))}
          </div>
          {error ? <ResourceError message={error} /> : null}
        </div>
      )}
    </section>
  )
}

function McpEditor({
  draft,
  saving,
  testing,
  inspection,
  onChange,
  onSave,
  onTest,
  onDelete,
}: {
  draft: McpDraft
  saving: boolean
  testing: boolean
  inspection: McpInspection | null
  onChange: (draft: McpDraft) => void
  onSave: () => void
  onTest: () => void
  onDelete: () => void
}) {
  return (
    <form className="resource-form-card" onSubmit={(event) => { event.preventDefault(); onSave() }}>
      <div className="resource-form-heading">
        <div>
          <p className="eyebrow">MCP server</p>
          <h2>{draft.originalName ? `编辑 ${draft.originalName}` : '新建 MCP 服务器'}</h2>
          <p>保存后从下一次 turn 开始生效。</p>
        </div>
        <span className="scope-badge">用户</span>
      </div>

      <div className="resource-field-grid">
        <label className="resource-field">
          <span>名称</span>
          <input value={draft.name} placeholder="my-mcp-server" onChange={(event) => onChange({ ...draft, name: event.target.value })} />
        </label>
        <label className="resource-field">
          <span>类型</span>
          <select value={draft.transport} onChange={(event) => onChange({ ...draft, transport: event.target.value as McpTransport })}>
            <option value="stdio">stdio（本地命令）</option>
            <option value="http">HTTP（远端服务）</option>
          </select>
        </label>
        <label className="resource-field">
          <span>启动超时（秒）</span>
          <input type="number" min={1} value={draft.startupTimeout} onChange={(event) => onChange({ ...draft, startupTimeout: Number(event.target.value) })} />
        </label>
        <label className="resource-field">
          <span>工具超时（秒）</span>
          <input type="number" min={1} value={draft.toolTimeout} onChange={(event) => onChange({ ...draft, toolTimeout: Number(event.target.value) })} />
        </label>
      </div>

      {draft.transport === 'stdio' ? (
        <div className="resource-field-grid">
          <label className="resource-field full">
            <span>命令</span>
            <input value={draft.command} placeholder="npx" onChange={(event) => onChange({ ...draft, command: event.target.value })} />
          </label>
          <label className="resource-field full">
            <span>参数（每行一个）</span>
            <textarea value={draft.argsText} placeholder={'-y\n@modelcontextprotocol/server-memory'} onChange={(event) => onChange({ ...draft, argsText: event.target.value })} />
          </label>
          <label className="resource-field full">
            <span>工作目录（可选）</span>
            <input value={draft.cwd} placeholder="默认使用当前工作区" onChange={(event) => onChange({ ...draft, cwd: event.target.value })} />
          </label>
          <SecretRows title="环境变量" rows={draft.env} onChange={(env) => onChange({ ...draft, env })} />
        </div>
      ) : (
        <div className="resource-field-grid">
          <label className="resource-field full">
            <span>URL</span>
            <input value={draft.url} placeholder="https://example.com/mcp" onChange={(event) => onChange({ ...draft, url: event.target.value })} />
          </label>
          <SecretRows title="HTTP Headers" rows={draft.headers} onChange={(headers) => onChange({ ...draft, headers })} />
        </div>
      )}

      <label className="resource-toggle-row">
        <input type="checkbox" checked={draft.enabled} onChange={(event) => onChange({ ...draft, enabled: event.target.checked })} />
        <span><strong>启用服务器</strong><small>停用后保留配置，但不会注入任何工具。</small></span>
      </label>

      <div className="resource-test-block">
        <button className="secondary-button" type="button" disabled={testing || saving} onClick={onTest}>
          <RefreshCw size={16} className={testing ? 'spinning' : undefined} /> {testing ? '测试中…' : '测试连接'}
        </button>
        <p>测试会执行本地命令或访问远端服务，但不会保存当前草稿。</p>
        {inspection ? (
          <div className={`resource-test-result${inspection.diagnostics.length ? ' warning' : ''}`}>
            <strong>发现 {inspection.tools.length} 个工具</strong>
            {inspection.tools.length ? <span>{inspection.tools.map((tool) => tool.name).join('、')}</span> : null}
            {inspection.diagnostics.map((diagnostic) => <span key={diagnostic}>{diagnostic}</span>)}
          </div>
        ) : null}
      </div>

      <div className="resource-form-actions split">
        {draft.originalName ? (
          <button className="danger-button subtle" type="button" disabled={saving} onClick={onDelete}>
            <Trash2 size={15} /> 删除服务器
          </button>
        ) : <span />}
        <button className="approve-button" type="submit" disabled={saving || testing}>
          <Save size={16} /> {saving ? '保存中…' : '保存服务器'}
        </button>
      </div>
    </form>
  )
}

function SecretRows({ title, rows, onChange }: { title: string; rows: SecretRow[]; onChange: (rows: SecretRow[]) => void }) {
  const update = (index: number, patch: Partial<SecretRow>) => onChange(rows.map((row, current) => current === index ? { ...row, ...patch } : row))
  return (
    <div className="resource-secret-section">
      <div className="resource-secret-heading">
        <span>{title}</span>
        <button type="button" onClick={() => onChange([...rows, { key: '', value: '' }])}><Plus size={14} /> 添加</button>
      </div>
      {rows.map((row, index) => (
        <div className="resource-secret-row" key={`${row.originalKey ?? 'new'}-${index}`}>
          <input aria-label={`${title} 名称`} value={row.key} placeholder="名称" onChange={(event) => update(index, { key: event.target.value })} />
          <input aria-label={`${title} 值`} value={row.value} type="password" autoComplete="off" placeholder={row.originalKey ? '留空以保留已保存值' : '值'} onChange={(event) => update(index, { value: event.target.value })} />
          <button type="button" title="删除" onClick={() => onChange(rows.filter((_, current) => current !== index))}><Trash2 size={15} /></button>
        </div>
      ))}
      {rows.length === 0 ? <span className="resource-secret-empty">未配置</span> : null}
    </div>
  )
}

function ReadOnlyServer({ server }: { server: McpServerResponse }) {
  return (
    <div className="resource-form-card readonly-resource">
      <div className="resource-form-heading">
        <div><p className="eyebrow">Runtime config</p><h2>{server.name}</h2><p>该服务器来自 morrow.toml，只读且敏感值不会发送到浏览器。</p></div>
        <span className={`resource-status ${server.enabled ? 'ready' : 'disabled'}`}>{server.enabled ? '已启用' : '已停用'}</span>
      </div>
      <dl className="settings-card settings-info-list">
        <div className="settings-info-row"><dt>类型</dt><dd>{server.transport}</dd></div>
        {server.command ? <div className="settings-info-row"><dt>命令</dt><dd>{server.command}</dd></div> : null}
        {server.url ? <div className="settings-info-row"><dt>URL</dt><dd>{server.url}</dd></div> : null}
        <div className="settings-info-row"><dt>参数</dt><dd>{server.args.length ? `${server.args.length} 项` : '无'}</dd></div>
        <div className="settings-info-row"><dt>敏感配置</dt><dd>{server.env_keys.length + server.http_header_keys.length} 项（已隐藏）</dd></div>
        <div className="settings-info-row"><dt>超时</dt><dd>{server.startup_timeout_sec}s / {server.tool_timeout_sec}s</dd></div>
      </dl>
    </div>
  )
}

function ResourceError({ message }: { message: string }) {
  return <div className="model-settings-error resource-error" role="alert"><CircleAlert size={17} /><span>{message}</span></div>
}

function emptyDraft(): McpDraft {
  return {
    originalName: null,
    name: '',
    transport: 'stdio',
    command: '',
    argsText: '',
    env: [],
    cwd: '',
    url: '',
    headers: [],
    enabled: true,
    startupTimeout: 10,
    toolTimeout: 60,
  }
}

function draftFromServer(server: McpServerResponse): McpDraft {
  return {
    originalName: server.name,
    name: server.name,
    transport: server.transport,
    command: server.command ?? '',
    argsText: server.args.join('\n'),
    env: server.env_keys.map((key) => ({ key, value: '', originalKey: key })),
    cwd: server.cwd ?? '',
    url: server.url ?? '',
    headers: server.http_header_keys.map((key) => ({ key, value: '', originalKey: key })),
    enabled: server.enabled,
    startupTimeout: server.startup_timeout_sec,
    toolTimeout: server.tool_timeout_sec,
  }
}

function requestFromDraft(draft: McpDraft): McpServerWriteRequest {
  return {
    name: draft.name.trim(),
    transport: draft.transport,
    command: draft.transport === 'stdio' ? draft.command.trim() : undefined,
    args: draft.transport === 'stdio' ? draft.argsText.split(/\r?\n/).map((arg) => arg.trim()).filter(Boolean) : [],
    env: draft.transport === 'stdio' ? secretRecord(draft.env) : {},
    cwd: draft.transport === 'stdio' ? draft.cwd.trim() || undefined : undefined,
    url: draft.transport === 'http' ? draft.url.trim() : undefined,
    http_headers: draft.transport === 'http' ? secretRecord(draft.headers) : {},
    enabled: draft.enabled,
    startup_timeout_sec: draft.startupTimeout,
    tool_timeout_sec: draft.toolTimeout,
  }
}

function secretRecord(rows: SecretRow[]): Record<string, string | null> {
  return Object.fromEntries(rows.filter((row) => row.key.trim()).map((row) => {
    const key = row.key.trim()
    const preserve = row.originalKey === key && !row.value
    return [key, preserve ? null : row.value]
  }))
}

function validateDraft(draft: McpDraft): string | null {
  if (!draft.name.trim()) return '服务器名称不能为空。'
  if (draft.startupTimeout <= 0 || draft.toolTimeout <= 0) return '超时时间必须大于零。'
  if (draft.transport === 'stdio' && !draft.command.trim()) return 'stdio 服务器命令不能为空。'
  if (draft.transport === 'http' && !/^https?:\/\//.test(draft.url.trim())) return 'HTTP URL 必须以 http:// 或 https:// 开头。'
  const rows = draft.transport === 'stdio' ? draft.env : draft.headers
  const seen = new Set<string>()
  for (const row of rows) {
    const key = row.key.trim()
    if (!key) return '敏感配置名称不能为空。'
    if (seen.has(key)) return `配置名称 ${key} 重复。`
    seen.add(key)
    if (row.originalKey !== key && !row.value) return `修改名称 ${key} 时必须重新填写值。`
  }
  return null
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
