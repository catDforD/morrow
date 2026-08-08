import { useState } from 'react'
import {
  CheckCircle2,
  CircleAlert,
  Clock3,
  Code2,
  ShieldAlert,
  ShieldCheck,
  Webhook,
} from 'lucide-react'
import { fetchJson } from './api'
import type {
  HookDefinitionStatus,
  HookEvent,
  HookSettingsResponse,
  MiddlewareAgentScope,
} from './types'

const eventLabels: Record<HookEvent, string> = {
  before_prompt: 'Before Prompt',
  before_tool: 'Before Tool',
  permission_request: 'Permission Request',
  after_tool: 'After Tool',
  pre_compact: 'Pre Compact',
  post_compact: 'Post Compact',
}

const scopeLabels: Record<MiddlewareAgentScope, string> = {
  main: '主 Agent',
  delegated_subagent: '临时子 Agent',
  persistent_subagent: '持久子 Agent',
}

export default function HookSettingsPanel({
  settings,
  onChanged,
}: {
  settings: HookSettingsResponse | null
  onChanged: () => Promise<void>
}) {
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const userHooks = settings?.hooks.filter((hook) => hook.source === 'user') ?? []
  const projectHooks = settings?.hooks.filter((hook) => hook.source === 'project') ?? []

  const updateTrust = async (action: 'trust' | 'revoke') => {
    if (
      action === 'trust' &&
      !window.confirm(
        '项目 Hook 会执行仓库控制的命令，并继承当前进程的完整环境，包括 API key。确认信任当前配置吗？',
      )
    ) {
      return
    }
    setSaving(true)
    setError(null)
    try {
      await fetchJson<HookSettingsResponse>(`/api/hooks/${action}`, {
        method: 'POST',
      })
      await onChanged()
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  return (
    <section
      className="settings-page resource-settings-page hook-settings-page"
      aria-labelledby="hook-settings-title"
    >
      <header className="settings-page-header resource-settings-header">
        <div>
          <p className="eyebrow">Settings</p>
          <h1 id="hook-settings-title">Hooks</h1>
          <p>查看命令 Hook 的匹配范围、执行策略和当前项目配置的信任状态。</p>
        </div>
      </header>

      <div className="settings-safety-note hook-security-note">
        <ShieldAlert size={24} />
        <div>
          <strong>项目 Hook 可以执行仓库控制的命令</strong>
          <p>
            获得信任后，命令会在工作区中运行，并继承宿主进程的完整环境变量，包括 API key。修改或重排项目 Hook 配置会自动撤销信任。
          </p>
        </div>
      </div>

      <HookSourceSection
        title="用户 Hooks"
        description="来自用户配置，默认可信并在项目 Hooks 之前运行。"
        path={settings?.user_config_path ?? '—'}
        hooks={userHooks}
        emptyMessage="用户配置中没有 Hook。"
      />

      <HookSourceSection
        title="项目 Hooks"
        description="仅在当前配置指纹获得本机信任后运行。"
        path={settings?.project_config_path ?? '—'}
        hooks={projectHooks}
        emptyMessage="当前工作区没有项目 Hook。"
        actions={
          settings?.project_fingerprint ? (
            settings.project_trusted ? (
              <button
                className="danger-button subtle"
                type="button"
                disabled={saving}
                onClick={() => void updateTrust('revoke')}
              >
                <ShieldAlert size={15} /> {saving ? '处理中…' : '撤销信任'}
              </button>
            ) : (
              <button
                className="approve-button"
                type="button"
                disabled={saving}
                onClick={() => void updateTrust('trust')}
              >
                <ShieldCheck size={15} /> {saving ? '处理中…' : '信任当前配置'}
              </button>
            )
          ) : null
        }
        fingerprint={settings?.project_fingerprint}
        trusted={settings?.project_trusted}
      />

      {settings?.diagnostics.length ? (
        <div className="resource-diagnostics" role="status">
          <CircleAlert size={17} />
          <span>{settings.diagnostics.join('\n')}</span>
        </div>
      ) : null}

      {error ? (
        <div className="model-settings-error resource-error" role="alert">
          <CircleAlert size={17} />
          <span>{error}</span>
        </div>
      ) : null}

      <dl className="settings-card settings-info-list hook-paths">
        <div className="settings-info-row">
          <dt>信任记录</dt>
          <dd className="path">{settings?.trust_store_path ?? '—'}</dd>
        </div>
      </dl>
    </section>
  )
}

function HookSourceSection({
  title,
  description,
  path,
  hooks,
  emptyMessage,
  actions,
  fingerprint,
  trusted,
}: {
  title: string
  description: string
  path: string
  hooks: HookDefinitionStatus[]
  emptyMessage: string
  actions?: React.ReactNode
  fingerprint?: string | null
  trusted?: boolean
}) {
  return (
    <div className="settings-section hook-source-section">
      <div className="settings-section-heading hook-section-heading">
        <div>
          <h2>{title}</h2>
          <p>{description}</p>
        </div>
        {actions}
      </div>
      <div className="hook-source-meta">
        <code>{path}</code>
        {fingerprint ? (
          <span className={`resource-status${trusted ? ' ready' : ''}`}>
            {trusted ? <CheckCircle2 size={12} /> : <ShieldAlert size={12} />}
            {trusted ? '已信任' : '未信任'}
          </span>
        ) : null}
      </div>
      {fingerprint ? (
        <p className="hook-fingerprint" title={fingerprint}>
          配置指纹：<code>{fingerprint}</code>
        </p>
      ) : null}
      <div className="resource-list-card hook-list-card">
        {hooks.length === 0 ? (
          <div className="resource-empty hook-empty">
            <Webhook size={28} />
            <strong>{emptyMessage}</strong>
          </div>
        ) : null}
        {hooks.map((hook) => (
          <HookRow hook={hook} key={`${hook.source}:${hook.id}`} />
        ))}
      </div>
    </div>
  )
}

function HookRow({ hook }: { hook: HookDefinitionStatus }) {
  const scopes = hook.agent_scopes?.map((scope) => scopeLabels[scope]).join('、') ?? '全部 Agent'
  const tools = hook.tool_names?.join('、') ?? '全部工具'
  return (
    <div className="hook-list-row">
      <span className="resource-list-icon">
        <Code2 size={18} />
      </span>
      <span className="resource-list-copy hook-list-copy">
        <span>
          <strong>{hook.id}</strong>
          <small>{eventLabels[hook.event]}</small>
          <small>{hook.failure_mode === 'open' ? 'Fail Open' : 'Fail Closed'}</small>
        </span>
        <code>{formatCommand(hook.command)}</code>
        <small>{scopes} · {tools}</small>
      </span>
      <span className="hook-row-status">
        <span className={`resource-status${hook.active ? ' ready' : ''}`}>
          {hook.active ? '运行中' : '已禁用'}
        </span>
        <small><Clock3 size={11} /> {hook.timeout_secs}s</small>
      </span>
    </div>
  )
}

function formatCommand(command: string[]): string {
  return command
    .map((part) => (/\s|["']/u.test(part) ? JSON.stringify(part) : part))
    .join(' ')
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
