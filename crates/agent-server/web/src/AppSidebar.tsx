import type { FormEvent, KeyboardEvent, ReactNode } from 'react'
import { useEffect, useRef, useState } from 'react'
import { Archive, ArchiveRestore, Check, ChevronDown, ChevronRight, Moon, PanelLeftClose, Plus, RefreshCw, Search, Settings, Sun, X } from 'lucide-react'
import { MiniIconButton } from './IconButton'
import { useDialogFocus } from './useDialogFocus'
import type { RunningTurnSnapshot, SessionEntryResponse } from './types'

export default function AppSidebar({
  sessions,
  archivedSessions,
  sessionCount,
  runningTurn,
  selected,
  sessionAction,
  isCreatingSession,
  newSessionName,
  createSessionError,
  isSearchOpen,
  sessionFilter,
  theme,
  searchInputRef,
  isHidden,
  onSelectSession,
  onStartCreateSession,
  onCancelCreateSession,
  onNewSessionNameChange,
  onCreateSession,
  onToggleSearch,
  onSessionFilterChange,
  onArchiveSession,
  onRestoreSession,
  onRefresh,
  onClose,
  onOpenSettings,
  onThemeToggle,
}: {
  sessions: SessionEntryResponse[]
  archivedSessions: SessionEntryResponse[]
  sessionCount: number
  runningTurn: RunningTurnSnapshot | null
  selected: string | null
  sessionAction: string | null
  isCreatingSession: boolean
  newSessionName: string
  createSessionError: string | null
  isSearchOpen: boolean
  sessionFilter: string
  theme: 'light' | 'dark'
  searchInputRef: React.RefObject<HTMLInputElement | null>
  isHidden: boolean
  onSelectSession: (name: string) => void
  onStartCreateSession: () => void
  onCancelCreateSession: () => void
  onNewSessionNameChange: (value: string) => void
  onCreateSession: () => void
  onToggleSearch: () => void
  onSessionFilterChange: (value: string) => void
  onArchiveSession: (name: string) => void
  onRestoreSession: (name: string) => void
  onRefresh: () => void
  onClose: () => void
  onOpenSettings: () => void
  onThemeToggle: () => void
}) {
  const sidebarRef = useRef<HTMLElement | null>(null)
  useDialogFocus(sidebarRef, !isHidden && window.matchMedia('(max-width: 900px)').matches ? 'navigation' : null)
  const [isArchiveOpen, setIsArchiveOpen] = useState(false)
  const selectedSessionRef = useRef<HTMLDivElement | null>(null)
  const showArchivedSessions = isArchiveOpen || sessionFilter.trim().length > 0

  useEffect(() => {
    selectedSessionRef.current?.scrollIntoView({ block: 'nearest' })
  }, [selected, sessions])

  return (
    <aside
      id="task-navigation"
      ref={sidebarRef}
      tabIndex={-1}
      className="app-sidebar workspace-sidebar"
      aria-label="会话导航"
      aria-hidden={isHidden}
      inert={isHidden}
    >
      <div className="sidebar-brand workspace-brand">
        <span className="workspace-brand-mark" aria-hidden="true">
          M
        </span>
        <strong className="workspace-brand-name">Morrow</strong>
        <MiniIconButton title="收起会话导航" onClick={onClose}>
          <PanelLeftClose size={17} />
        </MiniIconButton>
      </div>

      <nav className="sidebar-actions" aria-label="主导航">
        <SidebarAction
          icon={<Plus size={18} />}
          label="新建会话"
          onClick={onStartCreateSession}
        />
        <SidebarAction
          icon={<Search size={18} />}
          label="搜索"
          onClick={onToggleSearch}
        />
      </nav>

      <section className="session-browser" aria-label="会话">
        <div className="session-browser-head">
          <div>
            <p className="eyebrow">会话</p>
            <span>{sessionCount}</span>
          </div>
          <MiniIconButton title="新建会话" onClick={onStartCreateSession}>
            <Plus size={16} />
          </MiniIconButton>
        </div>

        {isSearchOpen ? (
          <label className="session-search">
            <Search size={16} />
            <input
              ref={searchInputRef}
              value={sessionFilter}
              placeholder="搜索会话"
              onChange={(event) => onSessionFilterChange(event.target.value)}
            />
          </label>
        ) : null}

        <div className="sidebar-session-list main-scroll">
          {isCreatingSession ? (
            <CreateSessionRow
              value={newSessionName}
              error={createSessionError}
              onChange={onNewSessionNameChange}
              onCancel={onCancelCreateSession}
              onSubmit={onCreateSession}
            />
          ) : null}
          {sessions.length === 0 ? (
            <p className="muted-line">
              {sessionFilter.trim() ? '没有匹配的会话' : '暂无会话'}
            </p>
          ) : (
            sessions.map((session) => (
              <div
                key={session.name}
                className={`sidebar-session-row${session.name === selected ? ' active' : ''}`}
                ref={session.name === selected ? selectedSessionRef : undefined}
              >
                <button
                  type="button"
                  className="sidebar-session"
                  aria-current={session.name === selected ? 'page' : undefined}
                  onClick={() => onSelectSession(session.name)}
                >
                  <span className="session-name">{session.name}</span>
                  <span>
                    {session.turns} 轮对话
                    {session.has_summary ? ' · 已压缩' : ''}
                  </span>
                </button>
                <button
                  type="button"
                  className="session-row-action archive-action"
                  title={
                    session.path
                      ? `归档会话 ${session.name}`
                      : '空会话无需归档'
                  }
                  aria-label={`归档会话 ${session.name}`}
                  disabled={
                    !session.path ||
                    Boolean(sessionAction) ||
                    (session.name === selected && Boolean(runningTurn))
                  }
                  onClick={() => onArchiveSession(session.name)}
                >
                  <Archive size={15} />
                </button>
              </div>
            ))
          )}

          {archivedSessions.length > 0 ? (
            <section className="archived-session-section" aria-label="已归档会话">
              <button
                type="button"
                className="archive-section-toggle"
                aria-expanded={showArchivedSessions}
                onClick={() => setIsArchiveOpen((open) => !open)}
              >
                <Archive size={14} />
                <span>已归档</span>
                <small>{archivedSessions.length}</small>
                {showArchivedSessions ? (
                  <ChevronDown size={14} />
                ) : (
                  <ChevronRight size={14} />
                )}
              </button>
              {showArchivedSessions ? (
                <div className="archived-session-list">
                  {archivedSessions.map((session) => (
                    <div className="sidebar-session-row archived" key={session.name}>
                      <div className="archived-session-copy">
                        <span className="session-name">{session.name}</span>
                        <span>{session.turns} 轮对话</span>
                      </div>
                      <button
                        type="button"
                        className="session-row-action restore-action"
                        title={`恢复会话 ${session.name}`}
                        aria-label={`恢复会话 ${session.name}`}
                        disabled={Boolean(sessionAction)}
                        onClick={() => onRestoreSession(session.name)}
                      >
                        <ArchiveRestore size={15} />
                      </button>
                    </div>
                  ))}
                </div>
              ) : null}
            </section>
          ) : null}
        </div>
      </section>

      <div className="sidebar-footer">
        <div className="sidebar-footer-row">
          <button
            className="sidebar-settings"
            type="button"
            title="打开设置"
            onClick={() => onOpenSettings()}
          >
            <Settings size={17} />
            <span>设置</span>
          </button>
          <div className="sidebar-footer-actions">
            <MiniIconButton title="刷新会话列表" onClick={onRefresh}>
              <RefreshCw size={16} />
            </MiniIconButton>
            <MiniIconButton title="切换主题" onClick={onThemeToggle}>
              {theme === 'dark' ? <Sun size={16} /> : <Moon size={16} />}
            </MiniIconButton>
          </div>
        </div>
      </div>
    </aside>
  )
}

function SidebarAction({ icon, label, onClick }: {
  icon: ReactNode
  label: string
  onClick: () => void
}) {
  return (
    <button className="sidebar-action" type="button" title={label} onClick={onClick}>
      {icon}
      <span>{label}</span>
    </button>
  )
}

function CreateSessionRow({
  value,
  error,
  onChange,
  onCancel,
  onSubmit,
}: {
  value: string
  error: string | null
  onChange: (value: string) => void
  onCancel: () => void
  onSubmit: () => void
}) {
  const canSubmit = value.trim().length > 0

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (canSubmit) onSubmit()
  }

  const handleKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'Escape') {
      event.preventDefault()
      onCancel()
    }
  }

  return (
    <form className="session-create-row" onSubmit={handleSubmit}>
      <input
        aria-label="新会话名称"
        autoFocus
        value={value}
        placeholder="会话名称，如 webui-redesign"
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={handleKeyDown}
      />
      <div className="session-create-actions">
        <MiniIconButton title="创建会话" type="submit" disabled={!canSubmit}>
          <Check size={17} />
        </MiniIconButton>
        <MiniIconButton title="取消" onClick={onCancel}>
          <X size={17} />
        </MiniIconButton>
      </div>
      {error ? <p>{error}</p> : null}
    </form>
  )
}
