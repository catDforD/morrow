import type { FormEvent, KeyboardEvent, ReactNode, UIEvent } from 'react'
import {
  createContext,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  useContext,
} from 'react'
import {
  ArrowDown,
  ArrowUp,
  Bot,
  Check,
  ChevronDown,
  ChevronRight,
  CheckCircle2,
  CircleAlert,
  Clock3,
  FileText,
  Folder,
  GitBranch,
  Eye,
  PanelLeft,
  PencilLine,
  Plus,
  RefreshCw,
  Send,
  Settings,
  Shield,
  ShieldCheck,
  Square,
  Terminal,
  X,
} from 'lucide-react'
import AppSidebar from './AppSidebar'
import ExecutionTrace from './ExecutionTrace'
import RunInspector from './RunInspector'
import SubagentInspector from './SubagentInspector'
import type { SubagentInspectorProps } from './SubagentInspector'
import TimelineNotices, { groupTimelineNotices } from './TimelineNotices'
import { IconButton, MiniIconButton } from './IconButton'
import MarkdownContent from './MarkdownContent'
import { useDialogFocus } from './useDialogFocus'
import { fetchJson } from './api'
import {
  isMessageScrollNearBottom,
  scrollMessageListToBottom,
} from './messageScroll'
import SettingsView from './SettingsView'
import type { SettingsSection, ThemePreference } from './SettingsView'
import {
  timelineFromSnapshot,
  toolsFromSnapshot,
} from './sessionTimelineReducer'
import { useSessionController } from './useSessionController'
import type {
  SessionSelectionStatus,
  WorkspaceSessionStatus,
} from './useSessionController'
import type {
  ApprovalRequest,
  ClientMessage,
  CommandSettingsResponse,
  HookSettingsResponse,
  Message,
  ModelSelection,
  ModelSettingsResponse,
  ModelProviderResponse,
  PermissionMode,
  ReasoningLevel,
  ResolveCommandResponse,
  RunningTurnSnapshot,
  SessionEntryResponse,
  StatusResponse,
  SubagentExecutionSummary,
  SubagentInstanceSnapshot,
  SubagentProfileResponse,
  SubagentRole,
  SubagentRunSummary,
  SubagentRunStatus,
  SubagentSettingsResponse,
  SubagentTranscriptSnapshot,
  TimelineItem,
  TimelineMessageItem,
  RunStep,
  RunTrace,
  ToolExecutionSummary,
  ToolRun,
} from './types'

type InspectorPanel = 'run' | 'subagents'
type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'
type AppView = 'workspace' | 'settings'
type ResolvedTheme = Exclude<ThemePreference, 'system'>

const SubagentProfilesContext = createContext<SubagentProfileResponse[]>([])
const PersistentSubagentsContext = createContext<SubagentInstanceSnapshot[]>([])

const permissionOptions: Array<{
  id: PermissionMode
  mode: PermissionMode
  label: string
  description: string
  disabled?: boolean
}> = [
  {
    id: 'read_only',
    mode: 'read_only',
    label: '只读模式',
    description: '仅检查和解释，不修改文件。',
  },
  {
    id: 'workspace_write',
    mode: 'workspace_write',
    label: '自动编辑',
    description: '可编辑工作区，敏感操作仍需确认。',
  },
  {
    id: 'danger_full_access',
    mode: 'danger_full_access',
    label: '完全访问',
    description: '允许工作区外操作并减少确认。',
  },
]

export default function App() {
  const initialLocationRef = useRef(readAppLocation())
  const [status, setStatus] = useState<StatusResponse | null>(null)
  const [appView, setAppView] = useState<AppView>(
    initialLocationRef.current.view,
  )
  const [settingsSection, setSettingsSection] = useState<SettingsSection>(
    initialLocationRef.current.section,
  )
  const [runCollapsed, setRunCollapsed] = useState<Record<string, { status: RunTrace['status']; collapsed: boolean }>>({})
  const [visibleError, setVisibleError] = useState<string | null>(null)
  const [visibleNotice, setVisibleNotice] = useState<string | null>(null)
  const [dismissedDiagnosticsKey, setDismissedDiagnosticsKey] = useState<
    string | null
  >(null)
  const [sessionFilter, setSessionFilter] = useState('')
  const [isSearchOpen, setIsSearchOpen] = useState(false)
  const [isSidebarOpen, setIsSidebarOpen] = useState(false)
  const [isSidebarCollapsed, setIsSidebarCollapsed] =
    useState(false)
  const [isNarrowViewport, setIsNarrowViewport] = useState(() =>
    window.matchMedia('(max-width: 900px)').matches,
  )
  const [inspectorPanel, setInspectorPanel] = useState<InspectorPanel>('run')
  const [isInspectorOpen, setIsInspectorOpen] = useState(false)
  const [isCreatingSession, setIsCreatingSession] = useState(false)
  const [newSessionName, setNewSessionName] = useState('')
  const [createSessionError, setCreateSessionError] = useState<string | null>(
    null,
  )
  const [subagentTranscript, setSubagentTranscript] =
    useState<SubagentTranscriptSnapshot | null>(null)
  const [prompt, setPrompt] = useState('')
  const [modelSettings, setModelSettings] =
    useState<ModelSettingsResponse | null>(null)
  const [commandSettings, setCommandSettings] =
    useState<CommandSettingsResponse | null>(null)
  const [hookSettings, setHookSettings] =
    useState<HookSettingsResponse | null>(null)
  const [subagentSettings, setSubagentSettings] =
    useState<SubagentSettingsResponse | null>(null)
  const [isResolvingCommand, setIsResolvingCommand] = useState(false)
  const [themePreference, setThemePreference] = useState<ThemePreference>(
    readSavedThemePreference,
  )
  const [systemTheme, setSystemTheme] = useState<ResolvedTheme>(
    readSystemTheme,
  )
  const savedPermissionModeRef = useRef(readSavedPermissionMode())
  const [permissionMode, setPermissionMode] = useState<PermissionMode>(
    savedPermissionModeRef.current ?? 'read_only',
  )
  const [sessionAction, setSessionAction] = useState<string | null>(null)

  const selectedRef = useRef<string | null>(initialLocationRef.current.session)
  const appViewRef = useRef(appView)
  const settingsSectionRef = useRef(settingsSection)
  const idRef = useRef(0)
  const messageScrollRef = useRef<HTMLDivElement | null>(null)
  const followMessagesRef = useRef(true)
  const sessionSearchRef = useRef<HTMLInputElement | null>(null)
  const resolvedTheme: ResolvedTheme =
    themePreference === 'system' ? systemTheme : themePreference

  const nextId = useCallback((prefix: string) => {
    idRef.current += 1
    return `${prefix}-${Date.now()}-${idRef.current}`
  }, [])

  const showError = useCallback(
    (error: unknown) => {
      const message = error instanceof Error ? error.message : String(error)
      setVisibleError(message)
    },
    [],
  )

  const sessionController = useSessionController({
    initialSession: initialLocationRef.current.session,
    onError: showError,
    onNotice: (message) => {
      setVisibleNotice(message)
    },
    onCommandData: (_requestId, data) => {
      setSubagentTranscript(data as SubagentTranscriptSnapshot)
    },
    onSelectionChange: (name) => {
      selectedRef.current = name
      followMessagesRef.current = true
      setIsSidebarOpen(false)
      setVisibleError(null)
      setVisibleNotice(null)
      setRunCollapsed({})
      writeAppLocation(
        name,
        appViewRef.current,
        settingsSectionRef.current,
        'replace',
        readHistoryState().fromWorkspace,
      )
    },
  })
  const {
    sessions,
    selected,
    timelineState: sessionTimelineState,
    pendingTurnRequest,
    modelSelection,
    selectionStatus,
    workspaceStatus,
    diagnostics,
    directoryError,
    sessionError,
  } = sessionController
  const diagnosticsKey = diagnostics
    .map(
      (diagnostic) =>
        `${diagnostic.name ?? ''}:${diagnostic.path}:${diagnostic.message}`,
    )
    .join('\n')
  const connection: ConnectionStatus =
    selectionStatus === 'ready'
      ? 'connected'
      : selectionStatus === 'subscribing' || selectionStatus === 'reconnecting'
        ? 'connecting'
        : 'disconnected'
  const sessionSnapshot = sessionTimelineState.snapshot
  const timeline = useMemo(
    () =>
      timelineFromSnapshot(sessionSnapshot).map((item) =>
        item.kind === 'run' && runCollapsed[item.id]?.status === item.trace.status
          ? {
              ...item,
              trace: { ...item.trace, collapsed: runCollapsed[item.id].collapsed },
            }
          : item,
      ),
    [runCollapsed, sessionSnapshot],
  )
  const tools = useMemo(() => toolsFromSnapshot(sessionSnapshot), [sessionSnapshot])
  const approvalQueue = sessionSnapshot?.approvals ?? []
  const pendingApproval = approvalQueue[0] ?? null
  const subagents = sessionSnapshot?.subagents ?? []
  const runningTurn: RunningTurnSnapshot | null = sessionSnapshot?.active_operation
    ? {
        turn_id: sessionSnapshot.active_operation.turn_id,
        pending_approval: pendingApproval?.id ?? null,
      }
    : null

  const selectSession = useCallback(async (name: string) => {
    setSubagentTranscript(null)
    await sessionController.selectSession(name)
  }, [sessionController.selectSession])

  const loadModelSettings = useCallback(async () => {
    const settings = await fetchJson<ModelSettingsResponse>('/api/model-settings')
    setModelSettings(settings)
    return settings
  }, [])

  const loadCommandSettings = useCallback(async () => {
    const settings = await fetchJson<CommandSettingsResponse>('/api/commands')
    setCommandSettings(settings)
    return settings
  }, [])

  const loadHookSettings = useCallback(async () => {
    const settings = await fetchJson<HookSettingsResponse>('/api/hooks')
    setHookSettings(settings)
    return settings
  }, [])

  const loadSubagentSettings = useCallback(async () => {
    const settings = await fetchJson<SubagentSettingsResponse>('/api/subagent-settings')
    setSubagentSettings(settings)
    return settings
  }, [])

  useEffect(() => {
    appViewRef.current = appView
  }, [appView])

  useEffect(() => {
    settingsSectionRef.current = settingsSection
  }, [settingsSection])

  useEffect(() => {
    const initial = initialLocationRef.current
    writeAppLocation(
      initial.session,
      initial.view,
      initial.section,
      'replace',
      false,
    )

    const handlePopState = () => {
      const next = readAppLocation()
      appViewRef.current = next.view
      settingsSectionRef.current = next.section
      setAppView(next.view)
      setSettingsSection(next.section)
      setIsSidebarOpen(false)
      setIsInspectorOpen(false)

      if (next.session !== selectedRef.current) {
        if (next.session) {
          void selectSession(next.session).catch(showError)
        } else {
          sessionController.clearSelection()
        }
      }
    }

    window.addEventListener('popstate', handlePopState)
    return () => window.removeEventListener('popstate', handlePopState)
  }, [selectSession, sessionController.clearSelection, showError])

  useEffect(() => {
    if (isSearchOpen) {
      sessionSearchRef.current?.focus()
    }
  }, [isSearchOpen])

  useEffect(() => {
    if (pendingApproval || (!isInspectorOpen && !isSidebarOpen)) return

    const handleKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape') {
        if (isInspectorOpen) {
          setIsInspectorOpen(false)
        } else {
          setIsSidebarOpen(false)
        }
      }
    }

    window.addEventListener('keydown', handleKeyDown)
    return () => window.removeEventListener('keydown', handleKeyDown)
  }, [isInspectorOpen, isSidebarOpen, pendingApproval])

  useEffect(() => {
    const media = window.matchMedia('(prefers-color-scheme: dark)')
    const handleChange = (event: MediaQueryListEvent) => {
      setSystemTheme(event.matches ? 'dark' : 'light')
    }

    setSystemTheme(media.matches ? 'dark' : 'light')
    media.addEventListener('change', handleChange)
    return () => media.removeEventListener('change', handleChange)
  }, [])

  useEffect(() => {
    document.documentElement.classList.toggle('dark', resolvedTheme === 'dark')
  }, [resolvedTheme])

  useEffect(() => {
    writeLocalPreference('morrow-theme', themePreference)
  }, [themePreference])

  useEffect(() => {
    const media = window.matchMedia('(max-width: 900px)')
    const handleChange = (event: MediaQueryListEvent) => {
      setIsNarrowViewport(event.matches)
      if (!event.matches) setIsSidebarOpen(false)
    }

    media.addEventListener('change', handleChange)
    return () => media.removeEventListener('change', handleChange)
  }, [])

  const handleMessageScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
    followMessagesRef.current = isMessageScrollNearBottom(event.currentTarget)
  }, [])

  useLayoutEffect(() => {
    if (appView !== 'workspace') return
    const scroller = messageScrollRef.current
    if (!scroller || !followMessagesRef.current) return
    scrollMessageListToBottom(scroller)
  }, [appView, timeline])

  useEffect(() => {
    let mounted = true
    async function boot() {
      try {
        const loadedStatus = await fetchJson<StatusResponse>('/api/status')
        if (!mounted) return
        setStatus(loadedStatus)
        const savedMode = savedPermissionModeRef.current
        setPermissionMode(savedMode ?? loadedStatus.permissions.mode)
      } catch (error) {
        if (mounted) showError(error)
      }
      void loadModelSettings().catch(showError)
      void loadCommandSettings().catch(showError)
      void loadHookSettings().catch(showError)
      void loadSubagentSettings().catch(showError)
    }

    void boot()
    return () => {
      mounted = false
    }
  }, [
    loadCommandSettings,
    loadHookSettings,
    loadModelSettings,
    loadSubagentSettings,
    showError,
  ])

  const selectedEntry = useMemo(
    () => sessions.find((session) => session.name === selected),
    [selected, sessions],
  )
  const filteredSessions = useMemo(() => {
    const query = sessionFilter.trim().toLowerCase()
    if (!query) return sessions
    return sessions.filter((session) =>
      session.name.toLowerCase().includes(query),
    )
  }, [sessionFilter, sessions])
  const activeSessions = useMemo(
    () => filteredSessions.filter((session) => !session.archived),
    [filteredSessions],
  )
  const archivedSessions = useMemo(
    () => filteredSessions.filter((session) => session.archived),
    [filteredSessions],
  )

  const isRunning = Boolean(runningTurn)
  const isSessionBusy = isRunning || pendingTurnRequest !== null
  const sessionCommandsEnabled = selectionStatus === 'ready'
  const selectedModel = useMemo(
    () => findSelectedModel(modelSettings, modelSelection),
    [modelSelection, modelSettings],
  )
  const canSend =
    sessionCommandsEnabled &&
    !isSessionBusy &&
    !isResolvingCommand &&
    prompt.trim().length > 0 &&
    Boolean(selectedModel)
  const canCancel = Boolean(
    sessionCommandsEnabled &&
      runningTurn?.turn_id &&
      runningTurn.turn_id !== 'pending',
  )
  const isWorkspaceSidebarCollapsed =
    !isNarrowViewport && isSidebarCollapsed
  const isWorkspaceSidebarVisible = isNarrowViewport
    ? isSidebarOpen
    : !isWorkspaceSidebarCollapsed

  const openInspector = (panel: InspectorPanel) => {
    setIsSidebarOpen(false)
    setInspectorPanel(panel)
    setIsInspectorOpen(true)
  }

  const openSidebar = () => {
    setIsInspectorOpen(false)
    if (isNarrowViewport) {
      setIsSidebarOpen(true)
    } else {
      setIsSidebarCollapsed(false)
    }
  }

  const openSettings = (section: SettingsSection = 'general') => {
    appViewRef.current = 'settings'
    settingsSectionRef.current = section
    setAppView('settings')
    setSettingsSection(section)
    setIsSidebarOpen(false)
    setIsInspectorOpen(false)
    writeAppLocation(
      selectedRef.current,
      'settings',
      section,
      'push',
      true,
    )
  }

  const closeSettings = () => {
    setIsSidebarOpen(false)
    const historyState = readHistoryState()
    if (historyState.morrowView === 'settings' && historyState.fromWorkspace) {
      history.back()
      return
    }

    appViewRef.current = 'workspace'
    settingsSectionRef.current = 'general'
    setAppView('workspace')
    setSettingsSection('general')
    writeAppLocation(
      selectedRef.current,
      'workspace',
      'general',
      'replace',
      false,
    )
  }

  const changeSettingsSection = (section: SettingsSection) => {
    settingsSectionRef.current = section
    setSettingsSection(section)
    setIsSidebarOpen(false)
    writeAppLocation(
      selectedRef.current,
      'settings',
      section,
      'replace',
      readHistoryState().fromWorkspace,
    )
  }

  const toggleSearch = () => {
    if (isSearchOpen) setSessionFilter('')
    setIsSearchOpen((open) => !open)
  }

  const handleSubmit = async (event: FormEvent) => {
    event.preventDefault()
    const trimmed = prompt.trim()
    if (!trimmed || !canSend) return
    setIsResolvingCommand(trimmed.startsWith('/'))
    try {
      const resolved = trimmed.startsWith('/')
        ? await fetchJson<ResolveCommandResponse>('/api/commands/resolve', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ input: trimmed }),
          })
        : { matched: false, prompt: trimmed }
      const resolvedPrompt = resolved.prompt.trim()
      if (!resolvedPrompt) throw new Error('resolved prompt must not be empty')
      followMessagesRef.current = true
      const requestId = `request-${Date.now()}`
      sessionController.send({
        type: 'start_turn',
        data: {
          request_id: requestId,
          prompt: resolvedPrompt,
          prompt_resolved: true,
          permission_mode: permissionMode,
          model_selection: modelSelection,
        },
      })
      setPrompt('')
      setVisibleError(null)
    } catch (error) {
      showError(error)
    } finally {
      setIsResolvingCommand(false)
    }
  }

  const handlePromptKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (
      shouldSubmitPromptOnEnter(
        event.key,
        event.ctrlKey,
        event.nativeEvent.isComposing || event.nativeEvent.keyCode === 229,
        event.shiftKey || event.altKey || event.metaKey,
      )
    ) {
      event.preventDefault()
      event.currentTarget.form?.requestSubmit()
    }
  }

  const startCreateSession = () => {
    setIsCreatingSession(true)
    setCreateSessionError(null)
  }

  const cancelCreateSession = () => {
    setIsCreatingSession(false)
    setNewSessionName('')
    setCreateSessionError(null)
  }

  const createSession = async () => {
    const name = newSessionName.trim()
    if (!name) return

    try {
      setCreateSessionError(null)
      await sessionController.createSession(name)
      setIsCreatingSession(false)
      setNewSessionName('')
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error)
      setCreateSessionError(message)
    }
  }

  const refresh = async () => {
    try {
      await sessionController.refreshSessions()
    } catch (error) {
      showError(error)
    }
  }

  const changePermissionMode = (mode: PermissionMode) => {
    setPermissionMode(mode)
    writeLocalPreference('morrow-permission-mode', mode)
  }

  const changeModelSelection = async (selection: ModelSelection) => {
    if (isSessionBusy) return
    try {
      await sessionController.changeModelSelection(selection)
    } catch (error) {
      showError(error)
    }
  }

  const archiveSession = async (name: string) => {
    if (sessionAction) return
    setSessionAction(`archive:${name}`)
    try {
      await sessionController.archiveSession(name)
    } catch (error) {
      showError(error)
    } finally {
      setSessionAction(null)
    }
  }

  const restoreSession = async (name: string) => {
    if (sessionAction) return
    setSessionAction(`restore:${name}`)
    try {
      await sessionController.restoreSession(name)
    } catch (error) {
      showError(error)
    } finally {
      setSessionAction(null)
    }
  }

  const cancelTurn = () => {
    if (!runningTurn || !canCancel) return
    try {
      sessionController.send({
        type: 'cancel_turn',
        data: { turn_id: runningTurn.turn_id },
      })
    } catch (error) {
      showError(error)
    }
  }

  const sendApproval = (approved: boolean) => {
    if (!pendingApproval || !sessionCommandsEnabled) return
    try {
      sessionController.send({
        type: 'approval_decision',
        data: {
          request_id: pendingApproval.id,
          approved,
        },
      })
    } catch (error) {
      showError(error)
    }
  }

  const sendSubagent = (instanceId: string, message: string) => {
    try {
      const invocation = subagentTranscript?.instance.id === instanceId
        ? subagentTranscript.model
        : null
      sessionController.send({
        type: 'send_subagent',
        data: {
          request_id: nextId('subagent-request'),
          instance_id: instanceId,
          message,
          model_selection: invocation ? {
            provider_id: invocation.provider_id,
            model_id: invocation.model_id,
            reasoning: invocation.reasoning,
          } : null,
        },
      })
    } catch (error) {
      showError(error)
    }
  }

  const inspectSubagent = (instanceId: string) => {
    try {
      sessionController.send({
        type: 'inspect_subagent',
        data: {
          request_id: nextId('inspect-subagent'),
          instance_id: instanceId,
        },
      })
    } catch (error) {
      showError(error)
    }
  }

  const cancelSubagent = (instanceId: string) => {
    try {
      sessionController.send({
        type: 'cancel_subagent',
        data: { instance_id: instanceId },
      })
    } catch (error) {
      showError(error)
    }
  }

  const deleteSubagent = (instanceId: string) => {
    try {
      sessionController.send({
        type: 'delete_subagent',
        data: { instance_id: instanceId },
      })
    } catch (error) {
      showError(error)
    }
  }

  const retrySession = () => {
    if (selected) {
      void selectSession(selected).catch(showError)
    } else {
      void refresh()
    }
  }

  const startCreateSessionFromWorkspace = () => {
    openSidebar()
    startCreateSession()
  }

  return (
    <SubagentProfilesContext.Provider value={subagentSettings?.profiles ?? []}>
      <PersistentSubagentsContext.Provider value={subagents}>
      <>
      <div style={{ height: '100%' }} inert={Boolean(pendingApproval)}>
      {appView === 'settings' ? (
          <SettingsView
            section={settingsSection}
            status={status}
            theme={themePreference}
            permissionMode={permissionMode}
            modelSettings={modelSettings}
            commandSettings={commandSettings}
            hookSettings={hookSettings}
            subagentSettings={subagentSettings}
            isSidebarOpen={isSidebarOpen}
            isSidebarHidden={isNarrowViewport && !isSidebarOpen}
            onSectionChange={changeSettingsSection}
            onBack={closeSettings}
            onOpenSidebar={() => setIsSidebarOpen(true)}
            onCloseSidebar={() => setIsSidebarOpen(false)}
            onThemeChange={setThemePreference}
            onPermissionModeChange={changePermissionMode}
            onModelSettingsChange={async () => {
              await loadModelSettings()
              if (selectedRef.current) {
                await selectSession(selectedRef.current)
              }
            }}
            onCommandSettingsChange={async () => {
              await loadCommandSettings()
            }}
            onHookSettingsChange={async () => {
              await loadHookSettings()
            }}
            onSubagentSettingsChange={async () => {
              await loadSubagentSettings()
            }}
          />
        ) : (
          <div
            className={`app-frame${isSidebarOpen ? ' sidebar-open' : ''}${isWorkspaceSidebarCollapsed ? ' sidebar-collapsed' : ''}${isInspectorOpen ? ' inspector-open' : ''}`}
          >
            <button
              className="mobile-sidebar-backdrop"
              type="button"
              aria-label="关闭会话导航"
              aria-hidden={!isSidebarOpen}
              tabIndex={isSidebarOpen ? 0 : -1}
              onClick={() => setIsSidebarOpen(false)}
            />
            <AppSidebar
              sessions={activeSessions}
              archivedSessions={archivedSessions}
              sessionCount={sessions.filter((session) => !session.archived).length}
              runningTurn={runningTurn}
              selected={selected}
              sessionAction={sessionAction}
              isCreatingSession={isCreatingSession}
              newSessionName={newSessionName}
              createSessionError={createSessionError}
              isSearchOpen={isSearchOpen}
              sessionFilter={sessionFilter}
              theme={resolvedTheme}
              searchInputRef={sessionSearchRef}
              isHidden={
                isWorkspaceSidebarCollapsed ||
                (isNarrowViewport && !isSidebarOpen)
              }
              onSelectSession={(name) => void selectSession(name)}
              onStartCreateSession={startCreateSession}
              onCancelCreateSession={cancelCreateSession}
              onNewSessionNameChange={setNewSessionName}
              onCreateSession={() => void createSession()}
              onToggleSearch={toggleSearch}
              onSessionFilterChange={setSessionFilter}
              onArchiveSession={(name) => void archiveSession(name)}
              onRestoreSession={(name) => void restoreSession(name)}
              onRefresh={() => void refresh()}
              onClose={() => {
                if (isNarrowViewport) {
                  setIsSidebarOpen(false)
                } else {
                  setIsSidebarCollapsed(true)
                }
              }}
              onOpenSettings={openSettings}
              onThemeToggle={() =>
                setThemePreference(resolvedTheme === 'dark' ? 'light' : 'dark')
              }
            />
            <main className="window-main">
              <ChatView
                selected={selected}
                status={status}
                connection={connection}
                workspaceStatus={workspaceStatus}
                selectionStatus={selectionStatus}
                directoryError={directoryError}
                sessionError={sessionError}
                visibleError={visibleError}
                visibleNotice={visibleNotice}
                diagnosticCount={
                  dismissedDiagnosticsKey === diagnosticsKey
                    ? 0
                    : diagnostics.length
                }
                timeline={timeline}
                runningTurn={runningTurn}
                pendingApproval={pendingApproval}
                prompt={prompt}
                canSend={canSend}
                canCancel={canCancel}
                isRunning={isSessionBusy}
                permissionMode={permissionMode}
                modelSettings={modelSettings}
                commandSettings={commandSettings}
                modelSelection={modelSelection}
                isResolvingCommand={isResolvingCommand}
                isSidebarOpen={isWorkspaceSidebarVisible}
                onPromptChange={setPrompt}
                onPromptKeyDown={handlePromptKeyDown}
                onSubmit={handleSubmit}
                onCancel={cancelTurn}
                onPermissionModeChange={changePermissionMode}
                onModelSelectionChange={(selection) =>
                  void changeModelSelection(selection)
                }
                onManageModels={() => openSettings('models')}
                onOpenSidebar={openSidebar}
                onStartCreateSession={startCreateSessionFromWorkspace}
                onRetryDirectory={() => void refresh()}
                onRetrySession={retrySession}
                onDismissStatus={() => {
                  setVisibleError(null)
                  setVisibleNotice(null)
                  setDismissedDiagnosticsKey(diagnosticsKey)
                }}
                onOpenInspector={openInspector}
                onToggleRun={(id) => {
                  const item = timeline.find(
                    (candidate) => candidate.kind === 'run' && candidate.id === id,
                  )
                  if (!item || item.kind !== 'run') return
                  setRunCollapsed((current) => ({
                    ...current,
                    [id]: { status: item.trace.status, collapsed: !item.trace.collapsed },
                  }))
                }}
                messageScrollRef={messageScrollRef}
                onMessageScroll={handleMessageScroll}
                onScrollToBottom={() => {
                  followMessagesRef.current = true
                  if (messageScrollRef.current) scrollMessageListToBottom(messageScrollRef.current)
                }}
              />
            </main>
            <InspectorDrawer
              open={isInspectorOpen}
              timeline={timeline}
              panel={inspectorPanel}
              selectedEntry={selectedEntry}
              runningTurn={runningTurn}
              pendingApproval={pendingApproval}
              approvalQueue={approvalQueue}
              subagents={subagents}
              subagentTranscript={subagentTranscript}
              onClose={() => setIsInspectorOpen(false)}
              onPanelChange={setInspectorPanel}
              onSendSubagent={sendSubagent}
              onInspectSubagent={inspectSubagent}
              onCancelSubagent={cancelSubagent}
              onDeleteSubagent={deleteSubagent}
              commandsEnabled={sessionCommandsEnabled}
            />
          </div>
        )}
      </div>
      <ApprovalDialog
        request={pendingApproval}
        disabled={!sessionCommandsEnabled}
        onApprove={() => sendApproval(true)}
        onDeny={() => sendApproval(false)}
      />
      </>
      </PersistentSubagentsContext.Provider>
    </SubagentProfilesContext.Provider>
  )
}

function ChatView({
  selected,
  status,
  connection,
  workspaceStatus,
  selectionStatus,
  directoryError,
  sessionError,
  visibleError,
  visibleNotice,
  diagnosticCount,
  timeline,
  runningTurn,
  pendingApproval,
  prompt,
  canSend,
  canCancel,
  isRunning,
  permissionMode,
  modelSettings,
  commandSettings,
  modelSelection,
  isResolvingCommand,
  isSidebarOpen,
  onPromptChange,
  onPromptKeyDown,
  onSubmit,
  onCancel,
  onPermissionModeChange,
  onModelSelectionChange,
  onManageModels,
  onOpenSidebar,
  onStartCreateSession,
  onRetryDirectory,
  onRetrySession,
  onDismissStatus,
  onOpenInspector,
  onToggleRun,
  messageScrollRef,
  onMessageScroll,
  onScrollToBottom,
}: {
  selected: string | null
  status: StatusResponse | null
  connection: ConnectionStatus
  workspaceStatus: WorkspaceSessionStatus
  selectionStatus: SessionSelectionStatus
  directoryError: string | null
  sessionError: string | null
  visibleError: string | null
  visibleNotice: string | null
  diagnosticCount: number
  timeline: TimelineItem[]
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
  prompt: string
  canSend: boolean
  canCancel: boolean
  isRunning: boolean
  permissionMode: PermissionMode
  modelSettings: ModelSettingsResponse | null
  commandSettings: CommandSettingsResponse | null
  modelSelection: ModelSelection | null
  isResolvingCommand: boolean
  isSidebarOpen: boolean
  onPromptChange: (value: string) => void
  onPromptKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => void
  onSubmit: (event: FormEvent) => void
  onCancel: () => void
  onPermissionModeChange: (mode: PermissionMode) => void
  onModelSelectionChange: (selection: ModelSelection) => void
  onManageModels: () => void
  onOpenSidebar: () => void
  onStartCreateSession: () => void
  onRetryDirectory: () => void
  onRetrySession: () => void
  onDismissStatus: () => void
  onOpenInspector: (panel: InspectorPanel) => void
  onToggleRun: (id: string) => void
  messageScrollRef: React.RefObject<HTMLDivElement | null>
  onMessageScroll: (event: UIEvent<HTMLDivElement>) => void
  onScrollToBottom: () => void
}) {
  const isEmpty = timeline.length === 0
  const composer = selected ? (
    <Composer
      prompt={prompt}
      enabled={selectionStatus === 'ready'}
      canSend={canSend}
      canCancel={canCancel}
      isRunning={isRunning}
      status={status}
      permissionMode={permissionMode}
      modelSettings={modelSettings}
      commandSettings={commandSettings}
      modelSelection={modelSelection}
      isResolvingCommand={isResolvingCommand}
      variant={isEmpty ? 'home' : 'dock'}
      onPromptChange={onPromptChange}
      onPromptKeyDown={onPromptKeyDown}
      onSubmit={onSubmit}
      onCancel={onCancel}
      onPermissionModeChange={onPermissionModeChange}
      onModelSelectionChange={onModelSelectionChange}
      onManageModels={onManageModels}
    />
  ) : null

  return (
    <section className={`conversation-panel${isEmpty ? ' home-mode' : ''}`}>
      <SessionStatusBanner
        workspaceStatus={workspaceStatus}
        selectionStatus={selectionStatus}
        directoryError={directoryError}
        sessionError={sessionError}
        visibleError={visibleError}
        visibleNotice={visibleNotice}
        diagnosticCount={diagnosticCount}
        hasSelection={selected !== null}
        onRetryDirectory={onRetryDirectory}
        onRetrySession={onRetrySession}
        onDismiss={onDismissStatus}
      />
      <ConversationHeader
        selected={selected}
        connection={connection}
        runningTurn={runningTurn}
        pendingApproval={pendingApproval}
        isSidebarOpen={isSidebarOpen}
        onOpenSidebar={onOpenSidebar}
        onOpenInspector={onOpenInspector}
      />
      {isEmpty ? (
        <>
          <HomePrompt
            status={status}
            hasSelection={selected !== null}
            onCreateSession={onStartCreateSession}
            onPickHint={selected ? onPromptChange : undefined}
          >
            {composer}
          </HomePrompt>
        </>
      ) : (
        <>
          <ConversationTimeline
            key={selected}
            onScrollToBottom={onScrollToBottom}
            items={timeline}
            messageScrollRef={messageScrollRef}
            onMessageScroll={onMessageScroll}
            onToggleRun={onToggleRun}
          />
          {composer}
        </>
      )}
    </section>
  )
}

function SessionStatusBanner({
  workspaceStatus,
  selectionStatus,
  directoryError,
  sessionError,
  visibleError,
  visibleNotice,
  diagnosticCount,
  hasSelection,
  onRetryDirectory,
  onRetrySession,
  onDismiss,
}: {
  workspaceStatus: WorkspaceSessionStatus
  selectionStatus: SessionSelectionStatus
  directoryError: string | null
  sessionError: string | null
  visibleError: string | null
  visibleNotice: string | null
  diagnosticCount: number
  hasSelection: boolean
  onRetryDirectory: () => void
  onRetrySession: () => void
  onDismiss: () => void
}) {
  const status = (() => {
    if (visibleError) {
      return { tone: 'error', message: visibleError, dismissible: true } as const
    }
    if (sessionError) {
      return {
        tone: 'error',
        message: sessionError,
        action: hasSelection ? onRetrySession : undefined,
        actionLabel: hasSelection ? '重新连接' : undefined,
      } as const
    }
    if (directoryError) {
      return {
        tone: 'error',
        message: `无法加载会话列表： ${directoryError}`,
        action: onRetryDirectory,
        actionLabel: '重新加载',
      } as const
    }
    if (selectionStatus === 'reconnecting') {
      return {
        tone: 'warning',
        message: '连接已中断，正在恢复会话…',
      } as const
    }
    if (selectionStatus === 'subscribing') {
      return {
        tone: 'neutral',
        message: '正在打开会话…',
      } as const
    }
    if (workspaceStatus === 'loading') {
      return { tone: 'neutral', message: '正在加载会话…' } as const
    }
    if (workspaceStatus === 'degraded' && diagnosticCount > 0) {
      return {
        tone: 'warning',
        message: `已跳过 ${diagnosticCount} 个损坏的会话记录，其余会话仍可使用。`,
        dismissible: true,
      } as const
    }
    if (visibleNotice) {
      return {
        tone: 'neutral',
        message: visibleNotice,
        dismissible: true,
      } as const
    }
    return null
  })()

  if (!status) return null
  return (
    <div
      className={`session-status-banner ${status.tone}`}
      role={status.tone === 'error' ? 'alert' : 'status'}
    >
      <span>{status.message}</span>
      {'action' in status && status.action ? (
        <button type="button" onClick={status.action}>
          <RefreshCw size={14} />
          {status.actionLabel}
        </button>
      ) : null}
      {'dismissible' in status && status.dismissible ? (
        <MiniIconButton title="关闭提示" onClick={onDismiss}>
          <X size={14} />
        </MiniIconButton>
      ) : null}
    </div>
  )
}

const homeHints: { icon: ReactNode; label: string; prompt: string }[] = [
  {
    icon: <Eye size={14} />,
    label: '介绍这个项目',
    prompt: '介绍一下这个项目：主要结构、核心模块和它们之间的关系。',
  },
  {
    icon: <FileText size={14} />,
    label: '解释一段代码',
    prompt: '帮我解释这段代码的作用：',
  },
  {
    icon: <PencilLine size={14} />,
    label: '修一个 bug',
    prompt: '帮我定位并修复一个 bug：',
  },
  {
    icon: <Terminal size={14} />,
    label: '跑一下测试',
    prompt: '运行项目的测试，并总结失败原因。',
  },
]

function HomePrompt({
  status,
  hasSelection,
  onCreateSession,
  onPickHint,
  children,
}: {
  status: StatusResponse | null
  hasSelection: boolean
  onCreateSession: () => void
  onPickHint?: (prompt: string) => void
  children: ReactNode
}) {
  const workspace = status ? workspaceName(status.workspace_root) : '当前工作区'

  return (
    <div className="home-prompt">
      <div className="home-copy">
        <div className="home-mark" aria-hidden="true">
          <GitBranch size={32} strokeWidth={1.5} />
        </div>
        <h1>
          {hasSelection ? '今天想一起完成什么？' : '从一个想法开始。'}
        </h1>
        <span className="home-workspace" title={status?.workspace_root}>
          <Folder size={14} />{workspace}
        </span>
      </div>
      {children}
      {!hasSelection ? (
        <button
          className="approve-button home-create-task"
          type="button"
          onClick={onCreateSession}
        >
          <Plus size={17} />
          <span>新建会话</span>
        </button>
      ) : null}
      {onPickHint ? (
        <div className="home-hints" aria-label="快速开始">
          {homeHints.map((hint) => (
            <button
              key={hint.label}
              type="button"
              className="home-hint"
              onClick={() => {
                onPickHint(hint.prompt)
                requestAnimationFrame(() => {
                  document
                    .querySelector<HTMLTextAreaElement>('.composer textarea')
                    ?.focus()
                })
              }}
            >
              {hint.icon}
              {hint.label}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  )
}

function ConversationHeader({
  selected,
  connection,
  runningTurn,
  pendingApproval,
  isSidebarOpen,
  onOpenSidebar,
  onOpenInspector,
}: {
  selected: string | null
  connection: ConnectionStatus
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
  isSidebarOpen: boolean
  onOpenSidebar: () => void
  onOpenInspector: (panel: InspectorPanel) => void
}) {
  const connectionState = pendingApproval ? 'approval' : runningTurn ? 'running' : connection
  const connectionLabels = { connected: '已连接', connecting: '连接中', disconnected: '未连接', running: '正在执行', approval: '等待确认' }

  return (
    <header className="conversation-header">
      <div className="conversation-title">
        <button
          className="mobile-menu-button"
          type="button"
          aria-label="打开会话导航"
          aria-controls="task-navigation"
          aria-expanded={isSidebarOpen}
          onClick={onOpenSidebar}
        >
          <PanelLeft size={19} />
        </button>
        <h1 title={selected ?? '新会话'}>{selected ?? '新会话'}</h1>
      </div>
      <div className="conversation-actions">
        <span className={`connection-badge ${connectionState}`} title={connectionLabels[connectionState]} aria-label={connectionLabels[connectionState]}>
          <span />
          {connectionLabels[connectionState]}
        </span>
        <MiniIconButton title="查看执行详情" onClick={() => onOpenInspector('run')}>
          <Shield size={16} />
        </MiniIconButton>
        <MiniIconButton title="查看子智能体" onClick={() => onOpenInspector('subagents')}>
          <Bot size={16} />
        </MiniIconButton>
      </div>
    </header>
  )
}

function ConversationTimeline({
  items,
  messageScrollRef,
  onMessageScroll,
  onToggleRun,
  onScrollToBottom,
}: {
  items: TimelineItem[]
  messageScrollRef: React.RefObject<HTMLDivElement | null>
  onMessageScroll: (event: UIEvent<HTMLDivElement>) => void
  onToggleRun: (id: string) => void
  onScrollToBottom: () => void
}) {
  const [showScrollButton, setShowScrollButton] = useState(false)
  useLayoutEffect(() => {
    const scroller = messageScrollRef.current
    if (!scroller) return
    const update = () => setShowScrollButton(!isMessageScrollNearBottom(scroller))
    update()
    if (typeof ResizeObserver === 'undefined') return
    const observer = new ResizeObserver(update)
    observer.observe(scroller)
    return () => observer.disconnect()
  }, [items, messageScrollRef])

  return (
    <div className="conversation-body">
    <div
      ref={messageScrollRef}
      className="message-scroll main-scroll"
      onScroll={(event) => {
        onMessageScroll(event)
        setShowScrollButton(!isMessageScrollNearBottom(event.currentTarget))
      }}
    >
      <div className="message-column">
        {groupTimelineNotices(items).map((item) => {
          if (item.kind === 'message') {
            return <TimelineMessage key={item.id} message={item} />
          }
          if (item.kind === 'run') {
            return (
              <ExecutionTrace
                key={item.id}
                trace={item.trace}
                onToggle={() => onToggleRun(item.id)}
                renderSpecialStep={(step) => <RunStepRow step={step} />}
              />
            )
          }
          return <TimelineNotices key={item.id} notices={item.notices} />
        })}
      </div>
    </div>
    {showScrollButton ? (
      <button className="scroll-to-bottom" type="button" aria-label="回到底部" title="回到底部" onClick={() => {
        onScrollToBottom()
        setShowScrollButton(false)
      }}>
        <ArrowDown size={17} />
      </button>
    ) : null}
    </div>
  )
}

function TimelineMessage({ message }: { message: TimelineMessageItem }) {
  return (
    <article className={`message-row ${message.role}`}>
      <div className={`message-role ${message.role}`}>{message.role === 'user' ? '你' : message.role === 'assistant' ? 'Morrow' : message.role}</div>
      {message.role === 'assistant' ? (
        <MarkdownMessage content={message.content} />
      ) : (
        <pre className="message-bubble">{message.content}</pre>
      )}
    </article>
  )
}

function MarkdownMessage({ content }: { content: string }) {
  return <MarkdownContent content={content} className="message-bubble" />
}

function RunStepRow({ step }: { step: RunStep }) {
  const isSubagent = step.kind === 'subagent'
  if (step.kind === 'persistent_subagent') {
    return (
      <article className={`run-step persistent-subagent ${step.status}`}>
        <span className="persistent-agent-branch" aria-hidden="true">
          <GitBranch size={13} />
        </span>
        <div className="run-step-main">
          <PersistentSubagentStepCard step={step} />
        </div>
      </article>
    )
  }

  return (
    <article className={`run-step ${step.kind} ${step.status}`}>
      <div className="run-step-icon"><RunStepIcon step={step} /></div>
      <div className="run-step-main">
        {isSubagent ? (
          <SubagentStepDisclosure step={step} />
        ) : (
          <>
            <div className="run-step-head">
              <strong>{step.title}</strong>
              <span>{{ running: '执行中', ok: '已完成', error: '失败', approval: '待确认' }[step.status]}</span>
            </div>
            {step.detail ? <p>{step.detail}</p> : null}
            <RunStepDetails step={step} />
          </>
        )}
      </div>
    </article>
  )
}

export function PersistentSubagentStepCard({
  step,
  profiles: providedProfiles,
  instances: providedInstances,
}: {
  step: RunStep
  profiles?: SubagentProfileResponse[]
  instances?: SubagentInstanceSnapshot[]
}) {
  const contextProfiles = useContext(SubagentProfilesContext)
  const contextInstances = useContext(PersistentSubagentsContext)
  const profiles = providedProfiles ?? contextProfiles
  const instances = providedInstances ?? contextInstances
  const profile = findSubagentProfile(profiles, step.agentId, step.agentName)
  const currentInstance = step.instanceId
    ? instances.find((instance) => instance.id === step.instanceId)
    : undefined
  const name = step.agentName || (step.status === 'error'
    ? '子 Agent 启动失败'
    : '正在分配子 Agent')
  const role = currentInstance?.role ?? step.agentRole
  const agentStatus = currentInstance?.status ?? step.agentStatus
  const task = step.detail?.trim()
    || currentInstance?.latest_task?.trim()
    || '等待主 Agent 分配任务'
  const summary = matchingPersistentSubagentSummary(
    currentInstance?.latest_summary,
    task,
  )
  const status = persistentSubagentStatusLabel(
    summary?.status,
    agentStatus,
    step.status,
  )
  const statusTone = summary?.status ?? agentStatus ?? step.status
  const error = summary?.error?.trim()
    || (step.status === 'error' && !currentInstance ? step.detail?.trim() : undefined)
  const result = summary?.result?.trim()

  return (
    <details className={`persistent-agent-card ${statusTone}`}>
      <summary
        className="persistent-agent-summary"
        aria-label={`${name}，展开或折叠任务输入与输出`}
      >
        <span className="persistent-agent-identity">
          <span className="persistent-agent-avatar">
            <SubagentIdentityAvatar avatar={profile?.avatar_data_url} />
          </span>
          <span className="persistent-agent-name">
            <strong>{name}</strong>
            <small>
              {role ? (
                <>
                  <b>{subagentRoleLabel(role)}</b>
                  <span>职责：{subagentRoleResponsibility(role)}</span>
                </>
              ) : (
                '正在同步身份与职责'
              )}
            </small>
          </span>
          <span className="persistent-agent-state">
            <span className={`persistent-agent-status ${statusTone}`}>
              {status}
            </span>
            <ChevronRight
              className="persistent-agent-chevron"
              size={16}
              aria-hidden="true"
            />
          </span>
        </span>
      </summary>
      <div className="subagent-panel persistent-agent-panel">
        <section className="subagent-pane prompt-pane">
          <header>
            <strong>任务输入</strong>
            <span>主 Agent 指令</span>
          </header>
          <div
            className="subagent-pane-body subagent-prompt"
            tabIndex={0}
            aria-label={`${name}任务输入`}
          >
            {task}
          </div>
        </section>
        <section className={`subagent-pane output-pane${error ? ' failed' : ''}`}>
          <header>
            <strong>子 Agent 输出</strong>
            {summary ? (
              <span>{persistentSubagentExecutionMeta(summary)}</span>
            ) : null}
          </header>
          <div
            className="subagent-pane-body subagent-output"
            tabIndex={0}
            aria-label={`${name}输出`}
          >
            {error ? (
              <p className="subagent-error">{error}</p>
            ) : result ? (
              <MarkdownContent content={result} className="subagent-markdown" />
            ) : persistentSubagentIsWaiting(summary?.status, agentStatus, step.status) ? (
              <div className="subagent-waiting" role="status">
                <Clock3 size={16} />
                <span>{persistentSubagentWaitingMessage(currentInstance)}</span>
              </div>
            ) : (
              <p className="subagent-empty">
                此阶段暂未同步输出，可在 AGENTS 中查看完整记录。
              </p>
            )}
          </div>
        </section>
      </div>
    </details>
  )
}

export function SubagentStepDisclosure({ step }: { step: RunStep }) {
  return (
    <details className="subagent-step-disclosure">
      <summary
        className="run-step-head subagent-step-summary"
        aria-label={`${step.title}，展开或折叠详情`}
      >
        <strong>{step.title}</strong>
        <span className="subagent-step-state">
          {step.status}
          <ChevronRight size={14} aria-hidden="true" />
        </span>
      </summary>
      <SubagentStepPanel step={step} />
    </details>
  )
}

export function SubagentStepPanel({ step }: { step: RunStep }) {
  const summary = step.summary?.subagent
  const task = summary?.task || step.detail || '未提供提示词'
  const result = summary?.result?.trim()
  const error = summary?.error?.trim()

  return (
    <div className="subagent-panel">
      <section className="subagent-pane prompt-pane">
        <header>
          <strong>提示词</strong>
        </header>
        <div
          className="subagent-pane-body subagent-prompt"
          tabIndex={0}
          aria-label={`${step.title}提示词`}
        >
          {task}
        </div>
      </section>
      <section className={`subagent-pane output-pane${error ? ' failed' : ''}`}>
        <header>
          <strong>子智能体输出</strong>
          {summary ? <span>{subagentExecutionMeta(summary)}</span> : null}
        </header>
        <div
          className="subagent-pane-body subagent-output"
          tabIndex={0}
          aria-label={`${step.title}输出`}
        >
          {step.status === 'running' && !summary ? (
            <div className="subagent-waiting" role="status">
              <Clock3 size={16} />
              <span>等待子智能体返回结果…</span>
            </div>
          ) : error ? (
            <p className="subagent-error">{error}</p>
          ) : result ? (
            <MarkdownContent content={result} className="subagent-markdown" />
          ) : (
            <p className="subagent-empty">子智能体未返回内容。</p>
          )}
        </div>
      </section>
    </div>
  )
}

function subagentExecutionMeta(summary: SubagentExecutionSummary): string {
  const truncated = summary.truncated ? ' · 结果已截断' : ''
  return `${summary.model_calls} 次模型调用 · ${summary.tool_calls} 次只读工具${truncated}`
}

function subagentRoleLabel(role: SubagentRole): string {
  switch (role) {
    case 'explore':
      return 'Explore'
    case 'plan':
      return 'Plan'
    case 'worker':
      return 'Worker'
    case 'reviewer':
      return 'Reviewer'
  }
}

function subagentRoleResponsibility(role: SubagentRole): string {
  switch (role) {
    case 'explore':
      return '只读检索与代码探索'
    case 'plan':
      return '只读分析与实施规划'
    case 'worker':
      return '工作区实现与受控命令执行'
    case 'reviewer':
      return '独立审查与审批后检查'
  }
}

function persistentSubagentStatusLabel(
  runStatus: SubagentRunStatus | undefined,
  status: RunStep['agentStatus'],
  fallback: RunStep['status'],
): string {
  switch (runStatus) {
    case 'completed':
      return '已完成'
    case 'failed':
      return '失败'
    case 'cancelled':
      return '已取消'
    case 'interrupted':
      return '已中断'
    case 'queued':
      return '排队中'
    case 'running':
      return '执行中'
    case 'waiting_approval':
      return '等待审批'
    case undefined:
      break
  }

  switch (status) {
    case 'idle':
      return '空闲'
    case 'queued':
      return '排队中'
    case 'running':
      return '执行中'
    case 'waiting_approval':
      return '等待审批'
    case 'interrupted':
      return '已中断'
    case 'failed':
      return '失败'
    case 'cancelled':
      return '已取消'
    case undefined:
      return fallback === 'running'
        ? '启动中'
        : fallback === 'error'
          ? '失败'
          : '已启动'
  }
}

function matchingPersistentSubagentSummary(
  summary: SubagentRunSummary | null | undefined,
  task: string,
): SubagentRunSummary | undefined {
  if (!summary) return undefined
  return summary.task.trim() === task.trim() ? summary : undefined
}

function persistentSubagentExecutionMeta(summary: SubagentRunSummary): string {
  const fileChangeCount = summary.file_changes?.length ?? 0
  const fileChanges = fileChangeCount
    ? ` · ${fileChangeCount} 个文件变更`
    : ''
  const truncated = summary.truncated ? ' · 输出已截断' : ''
  return `${summary.model_calls} 次模型调用 · ${summary.tool_calls} 次工具调用${fileChanges}${truncated}`
}

function persistentSubagentIsWaiting(
  runStatus: SubagentRunStatus | undefined,
  status: RunStep['agentStatus'],
  fallback: RunStep['status'],
): boolean {
  if (runStatus) {
    return ['queued', 'running', 'waiting_approval'].includes(runStatus)
  }
  if (status) {
    return ['queued', 'running', 'waiting_approval'].includes(status)
  }
  return fallback === 'running'
}

function persistentSubagentWaitingMessage(
  instance: SubagentInstanceSnapshot | undefined,
): string {
  switch (instance?.status) {
    case 'queued':
      return instance.queue_reason?.trim()
        ? `任务排队中：${instance.queue_reason.trim()}`
        : '任务正在等待执行…'
    case 'waiting_approval':
      return '子 Agent 正在等待审批…'
    default:
      return '子 Agent 正在执行任务…'
  }
}

function RunStepDetails({ step }: { step: RunStep }) {
  const summary = step.summary
  if (!summary && !step.reasoning) return null

  return (
    <>
      {step.reasoning ? (
        <details className="run-step-details reasoning-details">
          <summary>思考过程</summary>
          <pre>{step.reasoning}</pre>
        </details>
      ) : null}
      {summary ? (
        <details className="run-step-details">
          <summary>Details</summary>
          {summary.shell ? (
        <pre>
          {[
            `command: ${summary.shell.command}`,
            summary.shell.exit_code == null
              ? 'exit: unavailable'
              : `exit: ${summary.shell.exit_code}`,
            `timed out: ${summary.shell.timed_out ? '已压缩' : '未压缩'}`,
            `stdout truncated: ${summary.shell.stdout_truncated ? '已压缩' : '未压缩'}`,
            `stderr truncated: ${summary.shell.stderr_truncated ? '已压缩' : '未压缩'}`,
          ].join('\n')}
        </pre>
          ) : null}
          {summary.files?.length ? (
        <div className="run-file-list">
          {summary.files.map((file) => (
            <span key={`${file.operation}-${file.path}`}>
              {file.operation}: {file.path}
            </span>
          ))}
        </div>
          ) : null}
          {summary.diff ? <pre>{summary.diff}</pre> : null}
          {summary.error ? <pre>{summary.error}</pre> : null}
        </details>
      ) : null}
    </>
  )
}

function Composer({
  prompt,
  enabled,
  canSend,
  canCancel,
  isRunning,
  status,
  permissionMode,
  modelSettings,
  commandSettings,
  modelSelection,
  isResolvingCommand,
  variant = 'dock',
  onPromptChange,
  onPromptKeyDown,
  onSubmit,
  onCancel,
  onPermissionModeChange,
  onModelSelectionChange,
  onManageModels,
}: {
  prompt: string
  enabled: boolean
  canSend: boolean
  canCancel: boolean
  isRunning: boolean
  status: StatusResponse | null
  permissionMode: PermissionMode
  modelSettings: ModelSettingsResponse | null
  commandSettings: CommandSettingsResponse | null
  modelSelection: ModelSelection | null
  isResolvingCommand: boolean
  variant?: 'home' | 'dock'
  onPromptChange: (value: string) => void
  onPromptKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => void
  onSubmit: (event: FormEvent) => void
  onCancel: () => void
  onPermissionModeChange: (mode: PermissionMode) => void
  onModelSelectionChange: (selection: ModelSelection) => void
  onManageModels: () => void
}) {
  const textareaRef = useRef<HTMLTextAreaElement | null>(null)
  useLayoutEffect(() => {
    const input = textareaRef.current
    if (!input) return
    input.style.height = 'auto'
    input.style.height = `${Math.min(input.scrollHeight, 220)}px`
  }, [prompt])
  const [commandIndex, setCommandIndex] = useState(0)
  const [dismissedCommandPrompt, setDismissedCommandPrompt] = useState('')
  const commandQuery = slashCommandQuery(prompt)
  const commandSuggestions = useMemo(() => {
    if (commandQuery == null) return []
    const normalized = commandQuery.toLowerCase()
    return (commandSettings?.commands ?? [])
      .filter((command) =>
        `${command.name} ${command.description}`
          .toLowerCase()
          .includes(normalized),
      )
      .slice(0, 8)
  }, [commandQuery, commandSettings])
  const commandMenuOpen =
    commandQuery != null &&
    commandSuggestions.length > 0 &&
    dismissedCommandPrompt !== prompt &&
    enabled &&
    !isRunning &&
    !isResolvingCommand

  useEffect(() => {
    setCommandIndex((current) =>
      Math.min(current, Math.max(commandSuggestions.length - 1, 0)),
    )
  }, [commandSuggestions.length])

  const selectCommand = (index: number) => {
    const command = commandSuggestions[index]
    if (!command) return
    onPromptChange(`/${command.name} `)
    setDismissedCommandPrompt('')
    setCommandIndex(0)
  }

  const handleComposerKeyDown = (
    event: KeyboardEvent<HTMLTextAreaElement>,
  ) => {
    if (event.nativeEvent.isComposing || event.nativeEvent.keyCode === 229) return
    if (commandMenuOpen) {
      if (event.key === 'ArrowDown') {
        event.preventDefault()
        setCommandIndex((current) =>
          (current + 1) % commandSuggestions.length,
        )
        return
      }
      if (event.key === 'ArrowUp') {
        event.preventDefault()
        setCommandIndex((current) =>
          (current - 1 + commandSuggestions.length) % commandSuggestions.length,
        )
        return
      }
      if (
        (event.key === 'Enter' && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey) ||
        event.key === 'Tab'
      ) {
        event.preventDefault()
        selectCommand(commandIndex)
        return
      }
      if (event.key === 'Escape') {
        event.preventDefault()
        setDismissedCommandPrompt(prompt)
        return
      }
    }
    onPromptKeyDown(event)
  }

  const primaryLabel = isRunning
    ? '停止执行'
    : isResolvingCommand
      ? '解析命令中'
      : '发送'
  const primaryDisabled = isRunning ? !canCancel : !canSend
  const workspace = status ? workspaceName(status.workspace_root) : '加载中'

  return (
    <form className={`composer ${variant}`} onSubmit={onSubmit}>
      <div className="composer-shell">
        <div className="composer-card">
          {commandMenuOpen ? (
            <div className="command-suggestion-menu" role="listbox" aria-label="命令建议">
              <div className="command-suggestion-heading">
                <span>命令</span>
              </div>
              {commandSuggestions.map((command, index) => (
                <button
                  className={index === commandIndex ? 'active' : undefined}
                  type="button"
                  role="option"
                  aria-selected={index === commandIndex}
                  key={command.name}
                  onMouseEnter={() => setCommandIndex(index)}
                  onClick={() => selectCommand(index)}
                >
                  <span className="command-suggestion-icon"><Terminal size={16} /></span>
                  <span>
                    <strong>/{command.name}</strong>
                    {command.description ? <small>{command.description}</small> : null}
                  </span>
                  {command.argument_hint ? <em>{command.argument_hint}</em> : null}
                </button>
              ))}
            </div>
          ) : null}
          <textarea
            ref={textareaRef}
            aria-label="消息"
            value={prompt}
            rows={variant === 'home' ? 3 : 2}
            disabled={!enabled || isRunning || isResolvingCommand}
            placeholder="描述你想完成的工作，输入 / 使用命令"
            title="Enter 发送 · Shift / Ctrl + Enter 换行"
            onChange={(event) => {
              setDismissedCommandPrompt('')
              setCommandIndex(0)
              onPromptChange(event.target.value)
            }}
            onKeyDown={handleComposerKeyDown}
          />
          <div className="composer-bar">
            <div className="composer-left">
              <PermissionPicker
                mode={permissionMode}
                disabled={!enabled || isRunning || isResolvingCommand}
                onChange={onPermissionModeChange}
              />
            </div>
            <div className="composer-primary">
              <ModelPicker
                settings={modelSettings}
                selection={modelSelection}
                disabled={!enabled || isRunning || isResolvingCommand}
                onChange={onModelSelectionChange}
                onManage={onManageModels}
              />
              <button
                aria-label={primaryLabel}
                className={`send-button composer-primary-button${isRunning ? ' stop-button' : ''}`}
                type={isRunning ? 'button' : 'submit'}
                disabled={primaryDisabled}
                onClick={isRunning ? onCancel : undefined}
              >
                {isRunning ? (
                  <Square size={17} />
                ) : isResolvingCommand ? (
                  <RefreshCw size={17} className="spinning" />
                ) : (
                  <ArrowUp size={18} />
                )}
              </button>
            </div>
          </div>
        </div>
        <div className="composer-context" aria-label="工作区">
          <span title={status?.workspace_root || ''}><Folder size={13} />{workspace}</span>
        </div>
      </div>
    </form>
  )
}

function PermissionPicker({
  mode,
  disabled,
  onChange,
}: {
  mode: PermissionMode
  disabled: boolean
  onChange: (mode: PermissionMode) => void
}) {
  const [open, setOpen] = useState(false)
  const pickerRef = useRef<HTMLDivElement | null>(null)
  const selectedOption = permissionOptions.find((option) => option.mode === mode)

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

  useEffect(() => {
    if (disabled) setOpen(false)
  }, [disabled])

  return (
    <div className={`permission-picker${open ? ' open' : ''}`} ref={pickerRef}>
      <button
        className="permission-trigger"
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
      >
        {permissionModeIcon(mode, 15)}
        <span className="permission-trigger-label">
          {selectedOption?.label || '只读模式'}
        </span>
        <ChevronDown size={14} />
      </button>
      {open ? (
        <div className="permission-menu" role="menu" aria-label="权限模式">
          {permissionOptions.map((option) => {
            const isSelected = option.mode === mode
            return (
              <button
                key={option.id}
                type="button"
                role="menuitemradio"
                aria-checked={isSelected}
                className={`permission-option ${option.id}${isSelected ? ' selected' : ''}`}
                disabled={option.disabled}
                title={
                  option.disabled ? '计划模式将在后续版本开放' : option.label
                }
                onClick={() => {
                  if (!option.mode) return
                  onChange(option.mode)
                  setOpen(false)
                }}
              >
                <span className="permission-option-icon">
                  {permissionOptionIcon(option.id)}
                </span>
                <span className="permission-option-copy">
                  <strong>{option.label}</strong>
                  <small>{option.description}</small>
                </span>
                {option.disabled ? (
                  <span className="permission-option-badge">即将推出</span>
                ) : isSelected ? (
                  <Check size={16} />
                ) : null}
              </button>
            )
          })}
        </div>
      ) : null}
    </div>
  )
}

function ModelPicker({
  settings,
  selection,
  disabled,
  onChange,
  onManage,
}: {
  settings: ModelSettingsResponse | null
  selection: ModelSelection | null
  disabled: boolean
  onChange: (selection: ModelSelection) => void
  onManage: () => void
}) {
  const [open, setOpen] = useState(false)
  const [modelListOpen, setModelListOpen] = useState(false)
  const pickerRef = useRef<HTMLDivElement | null>(null)
  const selected = findSelectedModel(settings, selection)
  const providers =
    settings?.providers.filter(
      (provider) => provider.enabled && provider.models.length > 0,
    ) ?? []

  useEffect(() => {
    if (!open) return
    const handlePointerDown = (event: globalThis.PointerEvent) => {
      if (!pickerRef.current?.contains(event.target as Node)) {
        setOpen(false)
        setModelListOpen(false)
      }
    }
    const handleKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.stopPropagation()
        pickerRef.current?.querySelector('button')?.focus()
        setOpen(false)
        setModelListOpen(false)
      }
    }
    document.addEventListener('pointerdown', handlePointerDown)
    document.addEventListener('keydown', handleKeyDown)
    return () => {
      document.removeEventListener('pointerdown', handlePointerDown)
      document.removeEventListener('keydown', handleKeyDown)
    }
  }, [open])

  useEffect(() => {
    if (disabled) {
      setOpen(false)
      setModelListOpen(false)
    }
  }, [disabled])

  const openOrManage = () => {
    if (providers.length === 0) {
      onManage()
      return
    }
    if (open) {
      setOpen(false)
      setModelListOpen(false)
    } else {
      setOpen(true)
    }
  }

  return (
    <div className={`model-picker${open ? ' open' : ''}`} ref={pickerRef}>
      <button
        className="composer-chip labeled model-trigger"
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={disabled}
        title={selected ? modelSelectionLabel(settings, selection) : '配置模型'}
        onClick={openOrManage}
      >
        <Bot size={15} />
        <span>
          {selected
            ? `${selected.model.name} · ${reasoningLabel(selection?.reasoning ?? 'off')}`
            : '配置模型'}
        </span>
        <ChevronDown size={14} />
      </button>

      {open ? (
        <div className="model-menu" role="menu" aria-label="模型与思考设置">
          {selected?.model.reasoning_profile === 'deepseek' && selection ? (
            <section className="model-menu-section">
              {(['off', 'high', 'max'] as ReasoningLevel[]).map((reasoning) => (
                <button
                  key={reasoning}
                  type="button"
                  role="menuitemradio"
                  aria-checked={selection.reasoning === reasoning}
                  className={selection.reasoning === reasoning ? 'selected' : ''}
                  onClick={() =>
                    onChange({ ...selection, reasoning })
                  }
                >
                  <span>{reasoningLabel(reasoning)}</span>
                  {selection.reasoning === reasoning ? <Check size={15} /> : null}
                </button>
              ))}
            </section>
          ) : null}

          <section className="model-menu-section model-list-section">
            <button
              className={`model-current-toggle${modelListOpen ? ' expanded' : ''}`}
              type="button"
              aria-haspopup="menu"
              aria-expanded={modelListOpen}
              onClick={() => setModelListOpen((current) => !current)}
            >
              <span>{selected?.model.name ?? '选择模型'}</span>
              <ChevronRight size={15} />
            </button>
            {modelListOpen ? (
              <div className="model-list-expanded">
                {providers.map((provider) => (
                  <div className="model-provider-group" key={provider.id}>
                    <small>{provider.name}</small>
                    {provider.models.map((model) => {
                      const isSelected =
                        selection?.provider_id === provider.id &&
                        selection.model_id === model.id
                      return (
                        <button
                          key={model.id}
                          type="button"
                          role="menuitemradio"
                          aria-checked={isSelected}
                          className={isSelected ? 'selected' : ''}
                          onClick={() => {
                            onChange({
                              provider_id: provider.id,
                              model_id: model.id,
                              reasoning:
                                model.reasoning_profile === 'deepseek'
                                  ? isSelected
                                    ? selection?.reasoning ?? 'high'
                                    : 'high'
                                  : 'off',
                            })
                            setModelListOpen(false)
                          }}
                        >
                          <span>
                            <strong>{model.name}</strong>
                            <small>{compactTokenCount(model.context_window_tokens)}</small>
                          </span>
                          {isSelected ? <Check size={15} /> : null}
                        </button>
                      )
                    })}
                  </div>
                ))}
              </div>
            ) : null}
          </section>

          <button
            className="model-manage-link"
            type="button"
            onClick={() => {
              setOpen(false)
              setModelListOpen(false)
              onManage()
            }}
          >
            <Settings size={15} />
            <span>管理模型</span>
          </button>
        </div>
      ) : null}
    </div>
  )
}

function permissionOptionIcon(
  id: PermissionMode,
  size = 17,
): ReactNode {
  switch (id) {
    case 'read_only':
      return <Eye size={size} />
    case 'workspace_write':
      return <PencilLine size={size} />
    case 'danger_full_access':
      return <ShieldCheck size={size} />
  }
}

function permissionModeIcon(mode: PermissionMode, size: number): ReactNode {
  return permissionOptionIcon(mode, size)
}

export function InspectorDrawer({
  open,
  panel,
  selectedEntry,
  timeline = [],
  runningTurn,
  pendingApproval,
  approvalQueue,
  subagents,
  subagentTranscript,
  onClose,
  onPanelChange,
  onSendSubagent,
  onInspectSubagent,
  onCancelSubagent,
  onDeleteSubagent,
  commandsEnabled = true,
}: {
  open: boolean
  panel: InspectorPanel
  selectedEntry?: SessionEntryResponse
  timeline?: TimelineItem[]
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
  approvalQueue: ApprovalRequest[]
  subagents: SubagentInstanceSnapshot[]
  subagentTranscript: SubagentTranscriptSnapshot | null
  onClose: () => void
  onPanelChange: (panel: InspectorPanel) => void
  onSendSubagent: (instanceId: string, message: string) => void
  onInspectSubagent: (instanceId: string) => void
  onCancelSubagent: (instanceId: string) => void
  onDeleteSubagent: (instanceId: string) => void
  commandsEnabled?: boolean
}) {
  const panelRef = useRef<HTMLElement | null>(null)
  useDialogFocus(panelRef, open ? 'inspector' : null, true)
  return (
    <aside
      className={`inspector-drawer${open ? ' open' : ''}`}
      aria-hidden={!open}
      inert={!open}
    >
      <button
        className="drawer-backdrop"
        type="button"
        aria-label="关闭详情"
        onClick={onClose}
      />
      <section className="drawer-panel main-scroll" aria-label={inspectorPanelTitle(panel)} ref={panelRef} tabIndex={-1}>
        <header className="drawer-header">
          <div>
            <h2>{inspectorPanelTitle(panel)}</h2>
          </div>
          <MiniIconButton title="关闭详情" onClick={onClose}>
            <X size={18} />
          </MiniIconButton>
        </header>
        <nav className="drawer-tabs" aria-label="详情分类">
          <DrawerTab
            active={panel === 'run'}
            icon={<Shield size={16} />}
            label="执行"
            onClick={() => onPanelChange('run')}
          />
          <DrawerTab
            active={panel === 'subagents'}
            icon={<Bot size={16} />}
            label="子智能体"
            onClick={() => onPanelChange('subagents')}
          />
        </nav>
        <InspectorPanelContent
          panel={panel}
          selectedEntry={selectedEntry}
          timeline={timeline}
          runningTurn={runningTurn}
          pendingApproval={pendingApproval}
          approvalQueue={approvalQueue}
          subagents={subagents}
          subagentTranscript={subagentTranscript}
          onSendSubagent={onSendSubagent}
          onInspectSubagent={onInspectSubagent}
          onCancelSubagent={onCancelSubagent}
          onDeleteSubagent={onDeleteSubagent}
          commandsEnabled={commandsEnabled}
        />
      </section>
    </aside>
  )
}

function DrawerTab({
  active,
  icon,
  label,
  onClick,
}: {
  active: boolean
  icon: ReactNode
  label: string
  onClick: () => void
}) {
  return (
    <button
      className={`drawer-tab${active ? ' active' : ''}`}
      type="button"
      onClick={onClick}
    >
      {icon}
      <span>{label}</span>
    </button>
  )
}

function InspectorPanelContent({
  panel,
  selectedEntry,
  timeline = [],
  runningTurn,
  pendingApproval,
  approvalQueue,
  subagents,
  subagentTranscript,
  onSendSubagent,
  onInspectSubagent,
  onCancelSubagent,
  onDeleteSubagent,
  commandsEnabled,
}: {
  panel: InspectorPanel
  selectedEntry?: SessionEntryResponse
  timeline?: TimelineItem[]
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
  approvalQueue: ApprovalRequest[]
  subagents: SubagentInstanceSnapshot[]
  subagentTranscript: SubagentTranscriptSnapshot | null
  onSendSubagent: (instanceId: string, message: string) => void
  onInspectSubagent: (instanceId: string) => void
  onCancelSubagent: (instanceId: string) => void
  onDeleteSubagent: (instanceId: string) => void
  commandsEnabled: boolean
}) {
  if (panel === 'subagents') {
    return (
      <PersistentSubagentPanel
        instances={subagents}
        transcript={subagentTranscript}
        onSend={onSendSubagent}
        onInspect={onInspectSubagent}
        onCancel={onCancelSubagent}
        onDelete={onDeleteSubagent}
        commandsEnabled={commandsEnabled}
      />
    )
  }

  return (
    <RunInspector
      timeline={timeline}
      selectedEntry={selectedEntry}
      runningTurn={runningTurn}
      approvalQueue={approvalQueue.length ? approvalQueue : pendingApproval ? [pendingApproval] : []}
      renderApproval={(request) => <ApprovalBody request={request} />}
    />
  )
}

export function PersistentSubagentPanel(props: SubagentInspectorProps) {
  const profiles = useContext(SubagentProfilesContext)
  return <SubagentInspector {...props} renderAvatar={(instance) => {
    const profile = findSubagentProfile(profiles, instance.identity.id, instance.identity.name)
    return <SubagentIdentityAvatar avatar={profile?.avatar_data_url} />
  }} />
}

export function subagentTranscriptMessages(
  session: import('./types').SessionProjection,
): Message[] {
  return session.turns.flatMap((turn) => turn.messages)
}

function ToolList({
  tools,
  compact = false,
}: {
  tools: ToolRun[]
  compact?: boolean
}) {
  if (tools.length === 0) {
    return <p className="muted-line">还没有工具调用。</p>
  }

  return (
    <div className={`tool-list${compact ? ' compact' : ''}`}>
      {tools.map((tool) => (
        <article key={tool.id} className={`tool-card ${tool.status}`}>
          <div className="tool-card-head">
            <Terminal size={18} />
            <strong>{tool.name}</strong>
            <span>{tool.status}</span>
          </div>
          <p>{formatToolSummary(tool.summary)}</p>
          {!compact && tool.summary?.diff ? <pre>{tool.summary.diff}</pre> : null}
        </article>
      ))}
    </div>
  )
}

function ApprovalDialog({
  request,
  disabled,
  onApprove,
  onDeny,
}: {
  request: ApprovalRequest | null
  disabled: boolean
  onApprove: () => void
  onDeny: () => void
}) {
  const panelRef = useRef<HTMLElement | null>(null)
  useDialogFocus(panelRef, request?.id ?? null)
  if (!request) return null

  return (
    <div className="approval-overlay" role="dialog" aria-modal="true" aria-labelledby="approval-title">
      <section className="approval-panel" ref={panelRef} tabIndex={-1}>
        <header>
          <div>
            <p className="eyebrow">操作确认</p>
            <h2 id="approval-title">{approvalTitle(request)}</h2>
          </div>
          <IconButton title="拒绝" disabled={disabled} onClick={onDeny}>
            <X size={20} />
          </IconButton>
        </header>
        <p className="approval-source">来源：{approvalSource(request)}</p>
        <p className="approval-reason">{request.reason}</p>
        <ApprovalBody request={request} />
        <footer>
          <button
            className="danger-button"
            type="button"
            disabled={disabled}
            onClick={onDeny}
          >
            拒绝
          </button>
          <button
            className="approve-button"
            type="button"
            disabled={disabled}
            onClick={onApprove}
          >
            <CheckCircle2 size={18} />
            <span>批准</span>
          </button>
        </footer>
      </section>
    </div>
  )
}

function ApprovalBody({ request }: { request: ApprovalRequest }) {
  if (request.action.kind === 'shell_command') {
    return (
      <pre className="approval-body">
        {[
          `command: ${request.action.command}`,
          `cwd: ${request.action.cwd}`,
          `timeout: ${request.action.timeout_secs}s`,
        ].join('\n')}
      </pre>
    )
  }

  if (request.action.kind === 'mcp_tool') {
    return (
      <pre className="approval-body">
        {[
          `server: ${request.action.server}`,
          `tool: ${request.action.tool}`,
          `arguments: ${request.action.arguments}`,
        ].join('\n')}
      </pre>
    )
  }

  return (
    <div className="approval-files">
      <div className="file-list">
        {request.action.files.map((file) => (
          <span key={`${file.operation}-${file.path}`}>
            {file.operation}: {file.path}
          </span>
        ))}
      </div>
      <pre className="approval-body">{request.action.diff}</pre>
    </div>
  )
}

function formatToolSummary(summary?: ToolExecutionSummary): string {
  if (!summary) return 'running'
  if (summary.error) return summary.error
  if (summary.subagent) {
    if (summary.subagent.error) return summary.subagent.error
    const truncated = summary.subagent.truncated ? ' / truncated' : ''
    return `${summary.subagent.model_calls} model calls / ${summary.subagent.tool_calls} tools${truncated}`
  }
  const parts: string[] = []
  if (summary.shell) {
    parts.push(
      summary.shell.exit_code == null
        ? 'shell finished'
        : `exit ${summary.shell.exit_code}`,
    )
    if (summary.shell.timed_out) parts.push('timed out')
  }
  if (summary.files?.length) {
    parts.push(`${summary.files.length} files`)
  }
  if (summary.diff) {
    parts.push('diff available')
  }
  return parts.join(' / ') || 'finished'
}

function RunStepIcon({ step }: { step: RunStep }) {
  const profiles = useContext(SubagentProfilesContext)
  if (step.kind === 'subagent') {
    return <SubagentRunStepIcon step={step} profiles={profiles} />
  }
  return runStepIcon(step)
}

export function SubagentRunStepIcon({
  step,
  profiles,
}: {
  step: RunStep
  profiles: SubagentProfileResponse[]
}) {
  const profile = findSubagentProfile(profiles, step.agentId, step.agentName)
  return <SubagentIdentityAvatar avatar={profile?.avatar_data_url} />
}

function SubagentIdentityAvatar({ avatar }: { avatar?: string | null }) {
  const [imageFailed, setImageFailed] = useState(false)

  useEffect(() => setImageFailed(false), [avatar])

  if (avatar && !imageFailed) {
    return <img src={avatar} alt="" onError={() => setImageFailed(true)} />
  }
  return <Bot size={18} />
}

export function findSubagentProfile(
  profiles: SubagentProfileResponse[],
  agentId?: string,
  agentName?: string,
): SubagentProfileResponse | undefined {
  if (agentId) return profiles.find((profile) => profile.id === agentId)
  const normalizedName = agentName?.trim().toLocaleLowerCase()
  if (!normalizedName) return undefined
  return profiles.find(
    (profile) => profile.name.trim().toLocaleLowerCase() === normalizedName,
  )
}

function runStepIcon(step: RunStep): ReactNode {
  if (step.kind === 'subagent') return <Bot size={18} />
  if (step.status === 'running') return <Clock3 size={18} />
  if (step.status === 'error') return <CircleAlert size={18} />

  switch (step.kind) {
    case 'approval':
      return <Shield size={18} />
    case 'error':
      return <CircleAlert size={18} />
    case 'final':
      return <CheckCircle2 size={18} />
    case 'model':
      return <Bot size={18} />
    case 'persistent_subagent':
      return <GitBranch size={18} />
    case 'tool':
      return step.summary?.files?.length ? (
        <FileText size={18} />
      ) : (
        <Terminal size={18} />
      )
  }
}

function inspectorPanelTitle(panel: InspectorPanel): string {
  switch (panel) {
    case 'run':
      return '执行详情'
    case 'subagents':
      return '子智能体'
  }
}

function approvalSource(request: ApprovalRequest): string {
  const origin = request.origin
  if (!origin || origin.kind === 'unknown') return '主智能体'
  if (origin.kind === 'parent_turn') return origin.turn_id ?? 'parent turn'
  return `${origin.identity_name ?? origin.role} · ${origin.role} · ${origin.run_id}`
}

function approvalTitle(request: ApprovalRequest): string {
  switch (request.action.kind) {
    case 'shell_command':
      return '执行命令'
    case 'mcp_tool':
      return '调用 MCP 工具'
    default:
      return '修改文件'
  }
}

function compactText(text: string, length: number): string {
  if (text.length <= length) return text
  return `${text.slice(0, length - 1)}...`
}

function findSelectedModel(
  settings: ModelSettingsResponse | null,
  selection: ModelSelection | null,
): { provider: ModelProviderResponse; model: ModelProviderResponse['models'][number] } | null {
  if (!settings || !selection) return null
  const provider = settings.providers.find(
    (candidate) => candidate.id === selection.provider_id && candidate.enabled,
  )
  const model = provider?.models.find(
    (candidate) => candidate.id === selection.model_id,
  )
  return provider && model ? { provider, model } : null
}

function modelSelectionLabel(
  settings: ModelSettingsResponse | null,
  selection: ModelSelection | null,
): string {
  const selected = findSelectedModel(settings, selection)
  if (!selected || !selection) return '未配置模型'
  return `${selected.provider.name} · ${selected.model.name} · ${reasoningLabel(selection.reasoning)}`
}

export function completeRunningModelStep(trace: RunTrace): RunTrace {
  let changed = false
  const steps = trace.steps.map((step) => {
    if (step.kind !== 'model' || step.status !== 'running') return step
    changed = true
    return { ...step, status: 'ok' as const }
  })
  return changed ? { ...trace, steps } : trace
}

export function modelStepPresentation(
  settings: ModelSettingsResponse | null,
  selection: ModelSelection | null,
): { title: string; detail?: string } {
  const selected = findSelectedModel(settings, selection)
  if (!selected || !selection) return { title: 'Model call' }
  return {
    title: selected.model.name,
    detail: `${selected.provider.name} · ${reasoningLabel(selection.reasoning)}`,
  }
}

export function shouldSubmitPromptOnEnter(
  key: string,
  ctrlKey: boolean,
  isComposing: boolean,
  modified = false,
): boolean {
  return key === 'Enter' && !ctrlKey && !isComposing && !modified
}

function slashCommandQuery(prompt: string): string | null {
  if (!prompt.startsWith('/') || prompt.startsWith('//')) return null
  const firstLine = prompt.split(/\r?\n/, 1)[0]
  if (/\s/.test(firstLine)) return null
  return firstLine.slice(1)
}

function reasoningLabel(reasoning: ReasoningLevel): string {
  switch (reasoning) {
    case 'off':
      return '关闭思考'
    case 'high':
      return '高'
    case 'max':
      return '最高'
  }
}

function compactTokenCount(tokens: number): string {
  if (tokens >= 1_000_000) return `${tokens / 1_000_000}M`
  if (tokens >= 1_000) return `${Math.round(tokens / 1_000)}K`
  return String(tokens)
}

function workspaceName(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts.at(-1) || path
}

type AppLocation = {
  session: string | null
  view: AppView
  section: SettingsSection
}

type MorrowHistoryState = {
  morrowView?: AppView
  fromWorkspace: boolean
}

function readAppLocation(): AppLocation {
  const params = new URLSearchParams(location.search)
  const view: AppView =
    params.get('view') === 'settings' ? 'settings' : 'workspace'
  const requestedSection = params.get('section')
  const section: SettingsSection =
    view === 'settings' &&
    (requestedSection === 'about' ||
      requestedSection === 'models' ||
      requestedSection === 'subagents' ||
      requestedSection === 'mcp' ||
      requestedSection === 'commands' ||
      requestedSection === 'hooks')
      ? requestedSection
      : 'general'

  return {
    session: params.get('session'),
    view,
    section,
  }
}

function readHistoryState(): MorrowHistoryState {
  const state = history.state
  if (!state || typeof state !== 'object') {
    return { fromWorkspace: false }
  }

  return {
    morrowView:
      state.morrowView === 'workspace' || state.morrowView === 'settings'
        ? state.morrowView
        : undefined,
    fromWorkspace: state.fromWorkspace === true,
  }
}

function writeAppLocation(
  session: string | null,
  view: AppView,
  section: SettingsSection,
  method: 'push' | 'replace',
  fromWorkspace: boolean,
): void {
  const params = new URLSearchParams()
  if (session) params.set('session', session)
  if (view === 'settings') {
    params.set('view', 'settings')
    params.set('section', section)
  }

  const query = params.toString()
  const url = query ? `${location.pathname}?${query}` : location.pathname
  const state: MorrowHistoryState = { morrowView: view, fromWorkspace }
  if (method === 'push') {
    history.pushState(state, '', url)
  } else {
    history.replaceState(state, '', url)
  }
}

function readSavedThemePreference(): ThemePreference {
  const saved = readLocalPreference('morrow-theme')
  return saved === 'dark' || saved === 'light' || saved === 'system'
    ? saved
    : 'light'
}

function readSystemTheme(): ResolvedTheme {
  return window.matchMedia('(prefers-color-scheme: dark)').matches
    ? 'dark'
    : 'light'
}

function readLocalPreference(key: string): string | null {
  try {
    return localStorage.getItem(key)
  } catch {
    return null
  }
}

function writeLocalPreference(key: string, value: string): void {
  try {
    localStorage.setItem(key, value)
  } catch {
    // Keep the in-memory preference when browser storage is unavailable.
  }
}

function readSavedPermissionMode(): PermissionMode | null {
  const saved = readLocalPreference('morrow-permission-mode')
  return saved === 'read_only' ||
    saved === 'workspace_write' ||
    saved === 'danger_full_access'
    ? saved
    : null
}
