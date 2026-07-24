import type { ReactNode } from 'react'
import { useEffect, useRef, useState } from 'react'
import {
  ArrowLeft,
  BarChart3,
  Bot,
  Check,
  ChevronDown,
  Database,
  Eye,
  Info,
  Languages,
  Monitor,
  Moon,
  Network,
  PanelLeft,
  PencilLine,
  Plug,
  Server,
  Settings2,
  ShieldCheck,
  Sparkles,
  Sun,
  Terminal,
  X,
} from 'lucide-react'
import type { PermissionMode, StatusResponse } from './types'
import type {
  CommandSettingsResponse,
  ModelSettingsResponse,
  SubagentSettingsResponse,
} from './types'
import CommandSettingsPanel from './CommandSettingsPanel'
import McpSettingsPanel from './McpSettingsPanel'
import ModelSettingsPanel from './ModelSettingsPanel'
import SubagentSettingsPanel from './SubagentSettingsPanel'

export type SettingsSection = 'general' | 'models' | 'subagents' | 'mcp' | 'commands' | 'about'
export type ThemePreference = 'system' | 'light' | 'dark'

type SettingsSelectOption<T extends string> = {
  value: T
  label: string
  icon: ReactNode
  tone?: 'danger'
}

const themeOptions: SettingsSelectOption<ThemePreference>[] = [
  { value: 'system', label: '跟随系统', icon: <Monitor size={17} /> },
  { value: 'dark', label: '深色', icon: <Moon size={17} /> },
  { value: 'light', label: '浅色', icon: <Sun size={17} /> },
]

const permissionSettingOptions: SettingsSelectOption<PermissionMode>[] = [
  { value: 'read_only', label: '只读模式', icon: <Eye size={17} /> },
  {
    value: 'workspace_write',
    label: '自动编辑',
    icon: <PencilLine size={17} />,
  },
  {
    value: 'danger_full_access',
    label: '完全访问',
    icon: <ShieldCheck size={17} />,
    tone: 'danger',
  },
]

type SettingsNavigationItem = {
  label: string
  icon: ReactNode
  section: SettingsSection | null
}

const navigationItems: SettingsNavigationItem[] = [
  {
    label: '常规',
    icon: <Settings2 size={18} />,
    section: 'general',
  },
  { label: '模型设置', icon: <Bot size={18} />, section: 'models' },
  { label: '技能', icon: <Sparkles size={18} />, section: null },
  { label: '子智能体', icon: <Network size={18} />, section: 'subagents' },
  { label: 'MCP 服务器', icon: <Server size={18} />, section: 'mcp' },
  { label: '插件管理', icon: <Plug size={18} />, section: null },
  { label: '命令', icon: <Terminal size={18} />, section: 'commands' },
  { label: '索引库', icon: <Database size={18} />, section: null },
  { label: '使用统计', icon: <BarChart3 size={18} />, section: null },
  { label: '关于', icon: <Info size={18} />, section: 'about' },
]

export default function SettingsView({
  section,
  status,
  theme,
  permissionMode,
  modelSettings,
  commandSettings,
  subagentSettings,
  isSidebarOpen,
  isSidebarHidden,
  onSectionChange,
  onBack,
  onOpenSidebar,
  onCloseSidebar,
  onThemeChange,
  onPermissionModeChange,
  onModelSettingsChange,
  onCommandSettingsChange,
  onSubagentSettingsChange,
}: {
  section: SettingsSection
  status: StatusResponse | null
  theme: ThemePreference
  permissionMode: PermissionMode
  modelSettings: ModelSettingsResponse | null
  commandSettings: CommandSettingsResponse | null
  subagentSettings: SubagentSettingsResponse | null
  isSidebarOpen: boolean
  isSidebarHidden: boolean
  onSectionChange: (section: SettingsSection) => void
  onBack: () => void
  onOpenSidebar: () => void
  onCloseSidebar: () => void
  onThemeChange: (theme: ThemePreference) => void
  onPermissionModeChange: (mode: PermissionMode) => void
  onModelSettingsChange: () => Promise<void>
  onCommandSettingsChange: () => Promise<void>
  onSubagentSettingsChange: () => Promise<void>
}) {
  const title =
    section === 'about'
      ? '关于'
      : section === 'models'
        ? '模型设置'
        : section === 'subagents'
          ? '子智能体'
        : section === 'mcp'
          ? 'MCP 服务器'
          : section === 'commands'
            ? '命令'
            : '常规'

  return (
    <div
      className={`app-frame settings-frame${isSidebarOpen ? ' sidebar-open' : ''}`}
    >
      <button
        className="mobile-sidebar-backdrop"
        type="button"
        aria-label="关闭设置导航"
        aria-hidden={!isSidebarOpen}
        tabIndex={isSidebarOpen ? 0 : -1}
        onClick={onCloseSidebar}
      />

      <aside
        id="settings-navigation"
        className="app-sidebar settings-sidebar"
        aria-label="设置导航"
        aria-hidden={isSidebarHidden}
        inert={isSidebarHidden}
      >
        <div className="sidebar-brand">
          <div className="brand-mark">M</div>
          <div className="sidebar-brand-copy">
            <strong>Morrow</strong>
            <span>Settings</span>
          </div>
          <SettingsIconButton title="关闭设置导航" onClick={onCloseSidebar}>
            <X size={17} />
          </SettingsIconButton>
        </div>

        <button
          className="settings-back-button"
          type="button"
          onClick={onBack}
        >
          <ArrowLeft size={18} />
          <span>返回工作区</span>
        </button>

        <nav
          className="settings-navigation main-scroll"
          aria-label="设置分类"
        >
          {navigationItems.map((item) => {
            const active = item.section === section
            const disabled = item.section === null

            return (
              <button
                className={`settings-nav-item${active ? ' active' : ''}`}
                type="button"
                key={item.label}
                title={disabled ? `${item.label}后续开放` : item.label}
                disabled={disabled}
                aria-current={active ? 'page' : undefined}
                onClick={() => {
                  if (item.section) onSectionChange(item.section)
                }}
              >
                {item.icon}
                <span>{item.label}</span>
                {disabled ? <small>Soon</small> : null}
              </button>
            )
          })}
        </nav>

        <div className="settings-sidebar-footer">
          <span>Local settings</span>
          <strong>{status ? `v${status.version}` : '—'}</strong>
        </div>
      </aside>

      <main className="window-main settings-main">
        <header className="settings-mobile-header">
          <button
            className="mobile-menu-button"
            type="button"
            aria-label="打开设置导航"
            aria-controls="settings-navigation"
            aria-expanded={isSidebarOpen}
            onClick={onOpenSidebar}
          >
            <PanelLeft size={19} />
          </button>
          <div>
            <p className="eyebrow">Settings</p>
            <strong>{title}</strong>
          </div>
          <SettingsIconButton title="返回工作区" onClick={onBack}>
            <ArrowLeft size={17} />
          </SettingsIconButton>
        </header>

        <div className="settings-scroll main-scroll">
          {section === 'about' ? (
            <AboutSettings status={status} />
          ) : section === 'models' ? (
            <ModelSettingsPanel
              settings={modelSettings}
              onChanged={onModelSettingsChange}
            />
          ) : section === 'mcp' ? (
            <McpSettingsPanel />
          ) : section === 'subagents' ? (
            <SubagentSettingsPanel
              settings={subagentSettings}
              modelSettings={modelSettings}
              onChanged={onSubagentSettingsChange}
            />
          ) : section === 'commands' ? (
            <CommandSettingsPanel
              settings={commandSettings}
              onChanged={onCommandSettingsChange}
            />
          ) : (
            <GeneralSettings
              theme={theme}
              permissionMode={permissionMode}
              onThemeChange={onThemeChange}
              onPermissionModeChange={onPermissionModeChange}
            />
          )}
        </div>
      </main>
    </div>
  )
}

function GeneralSettings({
  theme,
  permissionMode,
  onThemeChange,
  onPermissionModeChange,
}: {
  theme: ThemePreference
  permissionMode: PermissionMode
  onThemeChange: (theme: ThemePreference) => void
  onPermissionModeChange: (mode: PermissionMode) => void
}) {
  return (
    <section className="settings-page" aria-labelledby="general-settings-title">
      <SettingsPageHeader
        eyebrow="Settings"
        title="常规"
        description="管理当前浏览器中的界面偏好和 Agent 默认行为。"
      />

      <div className="settings-page-badges" aria-label="保存方式">
        <span>浏览器本地</span>
        <span>自动保存</span>
      </div>

      <div className="settings-section">
        <div className="settings-section-heading">
          <h2>界面</h2>
          <p>设置仅保存在当前浏览器，不会写入 morrow.toml。</p>
        </div>
        <div className="settings-card">
          <SettingsRow
            icon={<Monitor size={20} />}
            title="界面主题"
            description="选择固定主题，或跟随操作系统的浅色与深色外观。"
          >
            <SettingsSelect
              label="界面主题"
              value={theme}
              options={themeOptions}
              onChange={onThemeChange}
            />
          </SettingsRow>

          <SettingsRow
            icon={<Languages size={20} />}
            title="界面语言"
            description="后续将支持在中文、英文和系统语言之间切换。"
            disabled
          >
            <div className="settings-disabled-control" aria-disabled="true">
              <Languages size={17} />
              <span>系统默认</span>
              <small>Soon</small>
            </div>
          </SettingsRow>
        </div>
      </div>

      <div className="settings-section">
        <div className="settings-section-heading">
          <h2>Agent 行为</h2>
          <p>默认权限会同步到工作区输入框，并从下一次 turn 开始使用。</p>
        </div>
        <div className="settings-card">
          <SettingsRow
            icon={<ShieldCheck size={20} />}
            title="默认权限"
            description="正在运行的 turn 不受修改影响，完全访问模式应谨慎使用。"
          >
            <SettingsSelect
              label="默认权限"
              value={permissionMode}
              options={permissionSettingOptions}
              onChange={onPermissionModeChange}
              preferUpOnMobile
            />
          </SettingsRow>
        </div>
      </div>
    </section>
  )
}

function AboutSettings({ status }: { status: StatusResponse | null }) {
  return (
    <section className="settings-page" aria-labelledby="about-settings-title">
      <SettingsPageHeader
        eyebrow="Settings"
        title="关于"
        description="查看当前 Morrow 服务、工作区和本地配置位置。"
      />

      <div className="settings-section">
        <div className="settings-section-heading">
          <h2>应用信息</h2>
          <p>这些信息来自当前服务的只读状态接口。</p>
        </div>
        <dl className="settings-card settings-info-list">
          <SettingsInfo
            label="Morrow 版本"
            value={status ? `v${status.version}` : '加载中…'}
          />
          <SettingsInfo
            label="当前工作区"
            value={status?.workspace_root ?? '加载中…'}
            path
          />
          <SettingsInfo
            label="配置文件"
            value={
              status
                ? status.config_path ?? '未加载（Web 可独立配置模型）'
                : '加载中…'
            }
            path
          />
          <SettingsInfo
            label="Web 模型配置"
            value={status?.model_store_path ?? '加载中…'}
            path
          />
          <SettingsInfo
            label="Web MCP 配置"
            value={status?.mcp_store_path ?? '加载中…'}
            path
          />
          <SettingsInfo
            label="用户命令目录"
            value={status?.command_store_path ?? '加载中…'}
            path
          />
        </dl>
      </div>

      <div className="settings-safety-note">
        <ShieldCheck size={24} />
        <div>
          <strong>Local-first</strong>
          <p>
            设置页不会回传已保存的 API Key。Web 模型可即时更新；MCP
            和上下文设置仍需手动编辑配置并重启服务。
          </p>
        </div>
      </div>
    </section>
  )
}

function SettingsPageHeader({
  eyebrow,
  title,
  description,
}: {
  eyebrow: string
  title: string
  description: string
}) {
  return (
    <header className="settings-page-header">
      <p className="eyebrow">{eyebrow}</p>
      <h1 id={`${title === '关于' ? 'about' : 'general'}-settings-title`}>
        {title}
      </h1>
      <p>{description}</p>
    </header>
  )
}

function SettingsRow({
  icon,
  title,
  description,
  disabled = false,
  children,
}: {
  icon: ReactNode
  title: string
  description: string
  disabled?: boolean
  children: ReactNode
}) {
  return (
    <div className={`settings-row${disabled ? ' disabled' : ''}`}>
      <div className="settings-row-icon" aria-hidden="true">
        {icon}
      </div>
      <div className="settings-row-copy">
        <strong>{title}</strong>
        <p>{description}</p>
      </div>
      <div className="settings-row-control">{children}</div>
    </div>
  )
}

function SettingsSelect<T extends string>({
  label,
  value,
  options,
  onChange,
  preferUpOnMobile = false,
}: {
  label: string
  value: T
  options: SettingsSelectOption<T>[]
  onChange: (value: T) => void
  preferUpOnMobile?: boolean
}) {
  const [open, setOpen] = useState(false)
  const pickerRef = useRef<HTMLDivElement | null>(null)
  const selectedOption =
    options.find((option) => option.value === value) ?? options[0]

  useEffect(() => {
    if (!open) return
    const handlePointerDown = (event: globalThis.PointerEvent) => {
      if (!pickerRef.current?.contains(event.target as Node)) setOpen(false)
    }
    const handleKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false)
    }

    document.addEventListener('pointerdown', handlePointerDown)
    document.addEventListener('keydown', handleKeyDown)
    return () => {
      document.removeEventListener('pointerdown', handlePointerDown)
      document.removeEventListener('keydown', handleKeyDown)
    }
  }, [open])

  if (!selectedOption) return null

  return (
    <div
      className={`settings-picker${open ? ' open' : ''}${preferUpOnMobile ? ' prefer-up-mobile' : ''}`}
      ref={pickerRef}
    >
      <button
        className="settings-picker-trigger"
        type="button"
        aria-label={label}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen((current) => !current)}
      >
        <span className="settings-picker-trigger-icon">
          {selectedOption.icon}
        </span>
        <span>{selectedOption.label}</span>
        <ChevronDown size={15} />
      </button>

      {open ? (
        <div className="settings-picker-menu" role="listbox" aria-label={label}>
          {options.map((option) => {
            const selected = option.value === value
            return (
              <button
                className={`settings-picker-option${selected ? ' selected' : ''}${option.tone ? ` ${option.tone}` : ''}`}
                type="button"
                role="option"
                aria-selected={selected}
                key={option.value}
                onClick={() => {
                  onChange(option.value)
                  setOpen(false)
                }}
              >
                <span className="settings-picker-option-icon">{option.icon}</span>
                <span>{option.label}</span>
                {selected ? <Check size={16} /> : null}
              </button>
            )
          })}
        </div>
      ) : null}
    </div>
  )
}

function SettingsInfo({
  label,
  value,
  path = false,
}: {
  label: string
  value: string
  path?: boolean
}) {
  return (
    <div className="settings-info-row">
      <dt>{label}</dt>
      <dd className={path ? 'path' : undefined} title={value}>
        {value}
      </dd>
    </div>
  )
}

function SettingsIconButton({
  title,
  onClick,
  children,
}: {
  title: string
  onClick: () => void
  children: ReactNode
}) {
  return (
    <button
      className="mini-icon-button"
      type="button"
      title={title}
      onClick={onClick}
    >
      <span className="sr-only">{title}</span>
      {children}
    </button>
  )
}
