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
  Activity,
  Archive,
  ArchiveRestore,
  Bot,
  CalendarClock,
  Check,
  ChevronDown,
  ChevronRight,
  CheckCircle2,
  CircleAlert,
  Clock3,
  FileText,
  Folder,
  Eye,
  ListTree,
  Moon,
  PanelLeft,
  PanelLeftClose,
  PencilLine,
  Plug,
  Plus,
  RefreshCw,
  Search,
  Send,
  Settings,
  Shield,
  ShieldCheck,
  Square,
  Sun,
  Terminal,
  Wrench,
  X,
} from 'lucide-react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { fetchJson, openSessionConnection } from './api'
import type { SessionConnection } from './api'
import {
  getDesktopPlatform,
  getDesktopShellState,
  runDesktopAction,
} from './desktop'
import type { DesktopPlatform, DesktopShellState } from './desktop'
import DesktopShell from './DesktopShell'
import {
  isMessageScrollNearBottom,
  scrollMessageListToBottom,
} from './messageScroll'
import {
  finishedSubagentStep,
  runningSubagentStep,
  subagentHistory,
  subagentStepTitle,
} from './subagentTrace'
import SettingsView from './SettingsView'
import type { SettingsSection, ThemePreference } from './SettingsView'
import {
  ProjectsDialog,
  RemoteConnectionDialog,
  WorkspaceMenu,
} from './WorkspaceManager'
import type {
  ActivityItem,
  AgentEvent,
  ApprovalRequest,
  ClientMessage,
  CommandSettingsResponse,
  Message,
  ModelSelection,
  ModelSettingsResponse,
  ModelProviderResponse,
  PermissionMode,
  ReasoningLevel,
  ResolveCommandResponse,
  RunningTurnSnapshot,
  ServerMessage,
  Session,
  SessionArchiveResponse,
  SessionDocument,
  SessionEntryResponse,
  SessionModelSelectionResponse,
  StatusResponse,
  SubagentExecutionSummary,
  SubagentInstanceSnapshot,
  SubagentProfileResponse,
  SubagentRole,
  SubagentSettingsResponse,
  SubagentTranscriptSnapshot,
  TimelineItem,
  TimelineMessageItem,
  TimelineNoticeItem,
  RunStep,
  RunTrace,
  ToolExecutionSummary,
  ToolRun,
} from './types'

type InspectorPanel = 'run' | 'subagents' | 'tools' | 'recent'
type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'
type AppView = 'workspace' | 'settings'
type ResolvedTheme = Exclude<ThemePreference, 'system'>

const markdownPlugins = [remarkGfm]
const SubagentProfilesContext = createContext<SubagentProfileResponse[]>([])

const permissionOptions: Array<{
  id: PermissionMode | 'plan'
  mode: PermissionMode | null
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
    id: 'plan',
    mode: null,
    label: '计划模式',
    description: '先制定实施计划，再开始修改。',
    disabled: true,
  },
  {
    id: 'danger_full_access',
    mode: 'danger_full_access',
    label: '完全访问',
    description: '允许工作区外操作并减少确认。',
  },
]

const emptySessionEntry = (name: string): SessionEntryResponse => ({
  name,
  path: '',
  turns: 0,
  active_messages: 0,
  summarized_turns: 0,
  has_summary: false,
  archived: false,
})

export default function App() {
  const initialLocationRef = useRef(readAppLocation())
  const [status, setStatus] = useState<StatusResponse | null>(null)
  const [sessions, setSessions] = useState<SessionEntryResponse[]>([])
  const [selected, setSelected] = useState(initialLocationRef.current.session)
  const [appView, setAppView] = useState<AppView>(
    initialLocationRef.current.view,
  )
  const [settingsSection, setSettingsSection] = useState<SettingsSection>(
    initialLocationRef.current.section,
  )
  const [timeline, setTimeline] = useState<TimelineItem[]>([])
  const [tools, setTools] = useState<ToolRun[]>([])
  const [activity, setActivity] = useState<ActivityItem[]>([])
  const [sessionFilter, setSessionFilter] = useState('')
  const [isSearchOpen, setIsSearchOpen] = useState(false)
  const [isSidebarOpen, setIsSidebarOpen] = useState(false)
  const [isDesktopSidebarCollapsed, setIsDesktopSidebarCollapsed] =
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
  const [runningTurn, setRunningTurn] = useState<RunningTurnSnapshot | null>(
    null,
  )
  const [pendingApproval, setPendingApproval] =
    useState<ApprovalRequest | null>(null)
  const [approvalQueue, setApprovalQueue] = useState<ApprovalRequest[]>([])
  const [subagents, setSubagents] = useState<SubagentInstanceSnapshot[]>([])
  const [subagentTranscript, setSubagentTranscript] =
    useState<SubagentTranscriptSnapshot | null>(null)
  const [connection, setConnection] =
    useState<ConnectionStatus>('disconnected')
  const [prompt, setPrompt] = useState('')
  const [modelSettings, setModelSettings] =
    useState<ModelSettingsResponse | null>(null)
  const [commandSettings, setCommandSettings] =
    useState<CommandSettingsResponse | null>(null)
  const [subagentSettings, setSubagentSettings] =
    useState<SubagentSettingsResponse | null>(null)
  const [isResolvingCommand, setIsResolvingCommand] = useState(false)
  const [modelSelection, setModelSelection] = useState<ModelSelection | null>(
    null,
  )
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
  const desktopPlatform = getDesktopPlatform()
  const [desktopState, setDesktopState] = useState<DesktopShellState | null>(null)
  const [workspaceDialog, setWorkspaceDialog] = useState<'projects' | 'remote' | null>(null)
  const [workspaceAction, setWorkspaceAction] = useState<number | null>(null)

  const socketRef = useRef<SessionConnection | null>(null)
  const selectedRef = useRef(selected)
  const modelSelectionRef = useRef<ModelSelection | null>(null)
  const modelSettingsRef = useRef<ModelSettingsResponse | null>(null)
  const appViewRef = useRef(appView)
  const settingsSectionRef = useRef(settingsSection)
  const assistantMessageIdRef = useRef<string | null>(null)
  const runTraceIdRef = useRef<string | null>(null)
  const idRef = useRef(0)
  const selectionRef = useRef(0)
  const modelSelectionRequestRef = useRef(0)
  const messageScrollRef = useRef<HTMLDivElement | null>(null)
  const followMessagesRef = useRef(true)
  const sessionSearchRef = useRef<HTMLInputElement | null>(null)
  const resolvedTheme: ResolvedTheme =
    themePreference === 'system' ? systemTheme : themePreference

  const nextId = useCallback((prefix: string) => {
    idRef.current += 1
    return `${prefix}-${Date.now()}-${idRef.current}`
  }, [])

  const refreshDesktopState = useCallback(async () => {
    if (!desktopPlatform) return
    try {
      setDesktopState(await getDesktopShellState())
    } catch (error) {
      console.error('Could not read desktop project state', error)
    }
  }, [desktopPlatform])

  useEffect(() => {
    void refreshDesktopState()
  }, [refreshDesktopState])

  const openLocalWorkspace = () => {
    setWorkspaceDialog(null)
    void runDesktopAction({ type: 'open_folder' })
  }

  const openRemoteWorkspace = () => {
    setWorkspaceDialog('remote')
  }

  const reconnectWorkspace = async (index: number) => {
    setWorkspaceAction(index)
    try {
      await runDesktopAction({ type: 'open_recent', index })
    } catch (error) {
      showError(error)
    } finally {
      setWorkspaceAction(null)
    }
  }

  const recordActivity = useCallback(
    (
      title: string,
      detail: string | undefined,
      tone: ActivityItem['tone'],
    ) => {
      const item: ActivityItem = {
        id: nextId('activity'),
        title,
        detail,
        tone,
        time: new Date().toLocaleTimeString([], {
          hour: '2-digit',
          minute: '2-digit',
          second: '2-digit',
        }),
      }
      setActivity((items) => [...items.slice(-119), item])
    },
    [nextId],
  )

  const addTimelineMessage = useCallback(
    (role: TimelineMessageItem['role'], content: string) => {
      const id = nextId(role)
      setTimeline((items) => [...items, { kind: 'message', id, role, content }])
      return id
    },
    [nextId],
  )

  const addNotice = useCallback(
    (
      tone: TimelineNoticeItem['tone'],
      title: string,
      detail?: string,
    ) => {
      const id = nextId('notice')
      setTimeline((items) => [...items, { kind: 'notice', id, tone, title, detail }])
      return id
    },
    [nextId],
  )

  const updateRunTrace = useCallback(
    (id: string, update: (trace: RunTrace) => RunTrace) => {
      setTimeline((items) =>
        items.map((item) =>
          item.kind === 'run' && item.id === id
            ? { ...item, trace: update(item.trace) }
            : item,
        ),
      )
    },
    [],
  )

  const createRunTrace = useCallback(
    (title: string, detail?: string) => {
      const id = nextId('run')
      const step: RunStep = {
        id: nextId('step'),
        kind: 'model',
        status: 'running',
        title,
        detail,
      }
      const trace: RunTrace = {
        id,
        status: 'running',
        collapsed: false,
        startedAt: currentTime(),
        steps: [step],
        toolCount: 0,
      }
      runTraceIdRef.current = id
      setTimeline((items) => [...items, { kind: 'run', id, trace }])
      return id
    },
    [nextId],
  )

  const ensureRunTrace = useCallback(
    (title: string, detail?: string) => {
      if (runTraceIdRef.current) return runTraceIdRef.current
      return createRunTrace(title, detail)
    },
    [createRunTrace],
  )

  const refreshCurrentModelStep = useCallback(
    (title: string, detail?: string) => {
      const id = ensureRunTrace(title, detail)
      updateRunTrace(id, (trace) => {
        const firstRunningModel = trace.steps.findIndex(
          (step) => step.kind === 'model' && step.status === 'running',
        )
        if (firstRunningModel === -1) return trace
        const steps = [...trace.steps]
        steps[firstRunningModel] = { ...steps[firstRunningModel], title, detail }
        return { ...trace, status: 'running', collapsed: false, steps }
      })
    },
    [ensureRunTrace, updateRunTrace],
  )

  const ensureLiveModelStep = useCallback(
    (status: RunStep['status'] = 'running') => {
      const modelStep = modelStepPresentation(
        modelSettingsRef.current,
        modelSelectionRef.current,
      )
      const id = ensureRunTrace(modelStep.title, modelStep.detail)
      updateRunTrace(id, (trace) => {
        if (
          trace.steps.some(
            (step) => step.kind === 'model' && step.status === 'running',
          )
        ) {
          return trace
        }
        return {
          ...trace,
          steps: [
            ...trace.steps,
            {
              id: nextId('step'),
              kind: 'model',
              status,
              title: modelStep.title,
              detail: modelStep.detail,
            },
          ],
        }
      })
      return id
    },
    [ensureRunTrace, nextId, updateRunTrace],
  )

  const completeLiveModelStep = useCallback(() => {
    const modelStep = modelStepPresentation(
      modelSettingsRef.current,
      modelSelectionRef.current,
    )
    const id = ensureRunTrace(modelStep.title, modelStep.detail)
    updateRunTrace(id, completeRunningModelStep)
    return id
  }, [ensureRunTrace, updateRunTrace])

  const appendReasoningDelta = useCallback(
    (text: string) => {
      const id = ensureLiveModelStep()
      updateRunTrace(id, (trace) => {
        const index = trace.steps.findIndex(
          (step) => step.kind === 'model' && step.status === 'running',
        )
        if (index === -1) return trace
        const steps = [...trace.steps]
        const step = steps[index]
        steps[index] = { ...step, reasoning: (step.reasoning ?? '') + text }
        return { ...trace, steps }
      })
    },
    [ensureLiveModelStep, updateRunTrace],
  )

  const upsertRunStep = useCallback(
    (runId: string, nextStep: RunStep) => {
      updateRunTrace(runId, (trace) => {
        const existing = trace.steps.findIndex((step) => step.id === nextStep.id)
        const steps =
          existing === -1
            ? [...trace.steps, nextStep]
            : trace.steps.map((step) =>
                step.id === nextStep.id ? { ...step, ...nextStep } : step,
              )
        return {
          ...trace,
          collapsed: false,
          steps,
          toolCount: steps.filter((step) => step.kind === 'tool').length,
        }
      })
    },
    [updateRunTrace],
  )

  const startToolStep = useCallback(
    (id: string, name: string) => {
      const runId = completeLiveModelStep()
      upsertRunStep(runId, {
        id,
        kind: 'tool',
        status: 'running',
        title: name,
        detail: 'Tool call started',
      })
    },
    [completeLiveModelStep, upsertRunStep],
  )

  const startSubagentStep = useCallback(
    (
      id: string,
      agentId: string | undefined,
      agentName: string | undefined,
      task: string,
    ) => {
      const runId = completeLiveModelStep()
      upsertRunStep(runId, runningSubagentStep(id, agentId, agentName, task))
    },
    [completeLiveModelStep, upsertRunStep],
  )

  const finishToolStep = useCallback(
    (
      id: string,
      name: string,
      ok: boolean,
      summary?: ToolExecutionSummary,
    ) => {
      const runId = ensureRunTrace('Tool result received', name)
      upsertRunStep(runId, {
        id,
        kind: 'tool',
        status: ok ? 'ok' : 'error',
        title: name,
        detail: formatToolSummary(summary),
        summary,
      })
    },
    [ensureRunTrace, upsertRunStep],
  )

  const finishSubagentStep = useCallback(
    (id: string, ok: boolean, summary: SubagentExecutionSummary) => {
      const runId = ensureRunTrace('Subagent result received', summary.task)
      upsertRunStep(runId, finishedSubagentStep(id, ok, summary))
    },
    [ensureRunTrace, upsertRunStep],
  )

  const setApprovalStep = useCallback(
    (requestId: string, title: string, detail: string, approved?: boolean) => {
      const runId = ensureRunTrace('Approval requested', detail)
      upsertRunStep(runId, {
        id: `approval-${requestId}`,
        kind: 'approval',
        status: approved == null ? 'approval' : approved ? 'ok' : 'error',
        title,
        detail,
      })
      updateRunTrace(runId, (trace) => ({
        ...trace,
        status: approved == null ? 'approval' : approved ? 'running' : 'failed',
      }))
    },
    [ensureRunTrace, updateRunTrace, upsertRunStep],
  )

  const completeCurrentRun = useCallback(() => {
    const id = runTraceIdRef.current
    if (!id) return
    updateRunTrace(id, (trace) => {
      const hasFinalStep = trace.steps.some((step) => step.kind === 'final')
      const completedSteps = trace.steps.map((step) =>
        step.status === 'running' || step.status === 'approval'
          ? { ...step, status: 'ok' as const }
          : step,
      )
      const steps = hasFinalStep
        ? completedSteps
        : [
            ...completedSteps,
            {
              id: nextId('step'),
              kind: 'final' as const,
              status: 'ok' as const,
              title: 'Final response ready',
            },
          ]
      return {
        ...trace,
        status: 'completed',
        collapsed: true,
        completedAt: currentTime(),
        steps,
      }
    })
    runTraceIdRef.current = null
  }, [nextId, updateRunTrace])

  const failCurrentRun = useCallback(
    (message: string) => {
      const id = runTraceIdRef.current
      if (!id) {
        addNotice('error', 'Error', message)
        return
      }
      updateRunTrace(id, (trace) => ({
        ...trace,
        status: 'failed',
        collapsed: false,
        completedAt: currentTime(),
        steps: [
          ...trace.steps.map((step) =>
            step.status === 'running' || step.status === 'approval'
              ? { ...step, status: 'error' as const }
              : step,
          ),
          {
            id: nextId('step'),
            kind: 'error',
            status: 'error',
            title: 'Error',
            detail: message,
          },
        ],
      }))
      runTraceIdRef.current = null
    },
    [addNotice, nextId, updateRunTrace],
  )

  const showError = useCallback(
    (error: unknown) => {
      const message = error instanceof Error ? error.message : String(error)
      failCurrentRun(message)
      recordActivity('Error', message, 'error')
    },
    [failCurrentRun, recordActivity],
  )

  const appendAssistantDelta = useCallback(
    (text: string) => {
      ensureLiveModelStep()
      if (!assistantMessageIdRef.current) {
        const id = nextId('assistant')
        assistantMessageIdRef.current = id
        setTimeline((items) => [
          ...items,
          { kind: 'message', id, role: 'assistant', content: text },
        ])
        return
      }

      const id = assistantMessageIdRef.current
      setTimeline((items) =>
        items.map((item) =>
          item.kind === 'message' && item.id === id
            ? { ...item, content: item.content + text }
            : item,
        ),
      )
    },
    [ensureLiveModelStep, nextId],
  )

  const upsertTool = useCallback(
    (
      id: string,
      name: string,
      toolStatus: ToolRun['status'],
      summary?: ToolExecutionSummary,
    ) => {
      setTools((items) => {
        const existing = items.find((item) => item.id === id)
        if (!existing) {
          return [...items, { id, name, status: toolStatus, summary }]
        }
        return items.map((item) =>
          item.id === id
            ? { ...item, name, status: toolStatus, summary: summary ?? item.summary }
            : item,
        )
      })
    },
    [],
  )

  const loadSessions = useCallback(async () => {
    const entries = await fetchJson<SessionEntryResponse[]>('/api/sessions')
    const current = selectedRef.current
    const nextEntries = entries.some((session) => session.name === current)
      ? entries
      : [emptySessionEntry(current), ...entries]
    setSessions(nextEntries)
    return nextEntries
  }, [])

  const loadModelSettings = useCallback(async () => {
    const settings = await fetchJson<ModelSettingsResponse>('/api/model-settings')
    modelSettingsRef.current = settings
    setModelSettings(settings)
    return settings
  }, [])

  const loadCommandSettings = useCallback(async () => {
    const settings = await fetchJson<CommandSettingsResponse>('/api/commands')
    setCommandSettings(settings)
    return settings
  }, [])

  const loadSubagentSettings = useCallback(async () => {
    const settings = await fetchJson<SubagentSettingsResponse>('/api/subagent-settings')
    setSubagentSettings(settings)
    return settings
  }, [])

  const loadSessionModelSelection = useCallback(async (name: string) => {
    const response = await fetchJson<SessionModelSelectionResponse>(
      `/api/sessions/${encodeURIComponent(name)}/model-selection`,
    )
    return response.selection ?? null
  }, [])

  const sendSocketMessage = useCallback((message: ClientMessage) => {
    const socket = socketRef.current
    if (!socket || !socket.isOpen) {
      throw new Error('websocket is not connected')
    }
    socket.send(JSON.stringify(message))
  }, [])

  const handleAgentEvent = useCallback(
    (event: AgentEvent) => {
      switch (event.type) {
        case 'turn_started': {
          const modelStep = modelStepPresentation(
            modelSettingsRef.current,
            modelSelectionRef.current,
          )
          assistantMessageIdRef.current = null
          setTools([])
          refreshCurrentModelStep(modelStep.title, modelStep.detail)
          recordActivity('Turn started', selectedRef.current, 'running')
          break
        }
        case 'model_call_started':
          ensureLiveModelStep()
          break
        case 'warning':
          recordActivity('Warning', event.data, 'error')
          break
        case 'reasoning_delta':
          appendReasoningDelta(event.data)
          break
        case 'text_delta':
          appendAssistantDelta(event.data)
          break
        case 'agent_message':
          if (!assistantMessageIdRef.current && event.data.trim()) {
            addTimelineMessage('assistant', event.data)
          }
          assistantMessageIdRef.current = null
          break
        case 'subagent_started':
          upsertTool(
            event.data.id,
            subagentStepTitle(event.data.agent_name),
            'running',
          )
          startSubagentStep(
            event.data.id,
            event.data.agent_id,
            event.data.agent_name,
            event.data.task,
          )
          recordActivity(
            `${subagentStepTitle(event.data.agent_name)} 开始执行`,
            event.data.task,
            'running',
          )
          break
        case 'subagent_finished':
          upsertTool(
            event.data.id,
            subagentStepTitle(event.data.summary.agent_name),
            event.data.ok ? 'ok' : 'error',
            { subagent: event.data.summary },
          )
          finishSubagentStep(
            event.data.id,
            event.data.ok,
            event.data.summary,
          )
          recordActivity(
            `${subagentStepTitle(event.data.summary.agent_name)}${
              event.data.ok ? ' 已完成' : ' 执行失败'
            }`,
            event.data.summary.task,
            event.data.ok ? 'ok' : 'error',
          )
          break
        case 'subagent_updated':
          setSubagents((current) => {
            const next = current.filter((instance) => instance.id !== event.data.id)
            next.push(event.data)
            return next.sort((left, right) => left.created_at_ms - right.created_at_ms)
          })
          setSubagentTranscript((current) => current?.instance.id === event.data.id
            ? { ...current, instance: event.data }
            : current)
          break
        case 'tool_call_started':
          upsertTool(event.data.id, event.data.name, 'running')
          startToolStep(event.data.id, event.data.name)
          recordActivity('Tool started', event.data.name, 'running')
          break
        case 'tool_call_finished':
          upsertTool(
            event.data.id,
            event.data.name,
            event.data.ok ? 'ok' : 'error',
            event.data.summary,
          )
          finishToolStep(
            event.data.id,
            event.data.name,
            event.data.ok,
            event.data.summary,
          )
          recordActivity(
            event.data.ok ? 'Tool finished' : 'Tool failed',
            event.data.name,
            event.data.ok ? 'ok' : 'error',
          )
          break
        case 'approval_requested':
          setApprovalQueue((current) => current.some((request) => request.id === event.data.id)
            ? current
            : [...current, event.data])
          setPendingApproval(event.data)
          setApprovalStep(event.data.id, 'Approval requested', event.data.reason)
          recordActivity('Approval requested', event.data.reason, 'approval')
          break
        case 'approval_resolved':
          setApprovalQueue((current) => {
            const next = current.filter((request) => request.id !== event.data.request_id)
            setPendingApproval(next[0] ?? null)
            return next
          })
          setApprovalStep(
            event.data.request_id,
            event.data.approved ? 'Approval granted' : 'Approval denied',
            event.data.request_id,
            event.data.approved,
          )
          recordActivity(
            event.data.approved ? 'Approval granted' : 'Approval denied',
            event.data.request_id,
            event.data.approved ? 'ok' : 'error',
          )
          break
        case 'turn_completed':
          setRunningTurn(null)
          assistantMessageIdRef.current = null
          completeCurrentRun()
          recordActivity('Turn completed', selectedRef.current, 'ok')
          break
        case 'error':
          setRunningTurn(null)
          showError(event.data)
          break
      }
    },
    [
      addTimelineMessage,
      appendReasoningDelta,
      appendAssistantDelta,
      completeCurrentRun,
      finishSubagentStep,
      finishToolStep,
      ensureLiveModelStep,
      recordActivity,
      refreshCurrentModelStep,
      setApprovalStep,
      showError,
      startSubagentStep,
      startToolStep,
      upsertTool,
    ],
  )

  const handleServerMessage = useCallback(
    (message: ServerMessage) => {
      switch (message.type) {
        case 'snapshot':
          setRunningTurn(message.data.running_turn ?? null)
          setSubagents(message.data.subagents ?? [])
          setApprovalQueue(message.data.approvals ?? [])
          setPendingApproval(message.data.approvals?.[0] ?? null)
          break
        case 'agent_event': {
          const origin = message.data.origin
          const event = message.data.event
          const isChildEvent = origin?.kind === 'subagent_run'
          if (!isChildEvent || event.type === 'approval_requested' || event.type === 'approval_resolved') {
            handleAgentEvent(event)
          } else if (event.type === 'error') {
            recordActivity(
              `${origin.identity_name ?? origin.role} failed`,
              event.data,
              'error',
            )
          }
          break
        }
        case 'turn_saved':
          void loadSessions().catch(showError)
          setRunningTurn(null)
          recordActivity('Turn saved', `#${message.data.turn_index}`, 'ok')
          break
        case 'turn_rejected':
          setRunningTurn(null)
          showError(message.data.reason)
          break
        case 'approval_queue_updated':
          setApprovalQueue(message.data.approvals)
          setPendingApproval(message.data.approvals[0] ?? null)
          break
        case 'subagent_transcript':
          setSubagentTranscript(message.data.transcript)
          break
        case 'subagent_deleted':
          setSubagents((current) => current.filter(
            (instance) => instance.id !== message.data.instance_id,
          ))
          setSubagentTranscript((current) => current?.instance.id === message.data.instance_id
            ? null
            : current)
          break
        case 'subagent_rejected':
          showError(message.data.reason)
          break
        case 'error':
          showError(message.data.message)
          break
      }
    },
    [handleAgentEvent, loadSessions, recordActivity, showError],
  )

  const closeSocket = useCallback(() => {
    const socket = socketRef.current
    if (!socket) return
    socket.close()
    socketRef.current = null
  }, [])

  const openSocket = useCallback(
    (name: string) => {
      setConnection('connecting')
      void openSessionConnection(name, {
        onOpen: () => {
          if (selectedRef.current !== name) return
          setConnection('connected')
          recordActivity('Session connected', name, 'ok')
        },
        onClose: () => {
          if (selectedRef.current !== name) return
          setConnection('disconnected')
          setRunningTurn(null)
          recordActivity('Session disconnected', name, 'neutral')
        },
        onMessage: (message) => {
          if (selectedRef.current === name) {
            handleServerMessage(message as ServerMessage)
          }
        },
        onError: (error) => {
          if (selectedRef.current === name) showError(error)
        },
      })
        .then((connection) => {
          if (selectedRef.current !== name) {
            connection.close()
            return
          }
          socketRef.current = connection
        })
        .catch(showError)
    },
    [handleServerMessage, recordActivity, showError],
  )

  const selectSession = useCallback(
    async (name: string) => {
      const selectionId = selectionRef.current + 1
      selectionRef.current = selectionId
      modelSelectionRequestRef.current += 1
      selectedRef.current = name
      followMessagesRef.current = true
      setSelected(name)
      setIsSidebarOpen(false)
      setRunningTurn(null)
      setPendingApproval(null)
      setTools([])
      setActivity([])
      assistantMessageIdRef.current = null
      runTraceIdRef.current = null
      setTimeline([])
      modelSelectionRef.current = null
      setModelSelection(null)
      closeSocket()
      writeAppLocation(
        name,
        appViewRef.current,
        settingsSectionRef.current,
        'replace',
        readHistoryState().fromWorkspace,
      )

      try {
        const [document, selection] = await Promise.all([
          fetchJson<SessionDocument>(
            `/api/sessions/${encodeURIComponent(name)}`,
          ),
          loadSessionModelSelection(name),
        ])
        if (selectionRef.current !== selectionId) return
        modelSelectionRef.current = selection
        setModelSelection(selection)
        setTimeline(sessionTimeline(document.session))
        recordActivity(
          'Session loaded',
          `${document.session.turns.length} turns`,
          'ok',
        )
        openSocket(name)
        await loadSessions()
      } catch (error) {
        if (selectionRef.current === selectionId) {
          showError(error)
        }
      }
    },
    [
      closeSocket,
      loadSessionModelSelection,
      loadSessions,
      openSocket,
      recordActivity,
      showError,
    ],
  )

  useEffect(() => {
    selectedRef.current = selected
  }, [selected])

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
        void selectSession(next.session)
      }
    }

    window.addEventListener('popstate', handlePopState)
    return () => window.removeEventListener('popstate', handlePopState)
  }, [selectSession])

  useEffect(() => {
    if (isSearchOpen) {
      sessionSearchRef.current?.focus()
    }
  }, [isSearchOpen])

  useEffect(() => {
    if (!isInspectorOpen && !isSidebarOpen) return

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
  }, [isInspectorOpen, isSidebarOpen])

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
        await Promise.all([
          loadModelSettings(),
          loadCommandSettings(),
          loadSubagentSettings(),
        ])
        const requestedName = initialLocationRef.current.session
        const entries = await loadSessions()
        const requestedEntry = entries.find(
          (session) => session.name === requestedName,
        )
        const name = requestedEntry?.archived
          ? entries.find((session) => !session.archived)?.name ||
            nextAvailableSessionName(entries)
          : requestedName
        selectedRef.current = name
        await selectSession(name)
      } catch (error) {
        if (mounted) showError(error)
      }
    }

    void boot()
    return () => {
      mounted = false
      closeSocket()
    }
  }, [
    closeSocket,
    loadCommandSettings,
    loadModelSettings,
    loadSubagentSettings,
    loadSessions,
    selectSession,
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
  const selectedModel = useMemo(
    () => findSelectedModel(modelSettings, modelSelection),
    [modelSelection, modelSettings],
  )
  const canSend =
    connection === 'connected' &&
    !isRunning &&
    !isResolvingCommand &&
    prompt.trim().length > 0 &&
    Boolean(selectedModel)
  const canCancel = Boolean(runningTurn?.turn_id && runningTurn.turn_id !== 'pending')
  const isWorkspaceSidebarCollapsed =
    !isNarrowViewport && isDesktopSidebarCollapsed
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
      setIsDesktopSidebarCollapsed(false)
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
      const modelStep = modelStepPresentation(modelSettings, modelSelection)
      followMessagesRef.current = true
      addTimelineMessage('user', resolvedPrompt)
      createRunTrace(modelStep.title, modelStep.detail)
      sendSocketMessage({
        type: 'start_turn',
        data: {
          request_id: `request-${Date.now()}`,
          prompt: resolvedPrompt,
          prompt_resolved: true,
          permission_mode: permissionMode,
          model_selection: modelSelection,
        },
      })
      setPrompt('')
      setRunningTurn({ turn_id: 'pending' })
      if (resolved.matched && resolved.command_name) {
        recordActivity(
          'Command expanded',
          `/${resolved.command_name}`,
          'neutral',
        )
      }
      recordActivity(
        'Turn requested',
        compactText(resolvedPrompt, 90),
        'running',
      )
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
        event.nativeEvent.isComposing,
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
      await fetchJson<SessionDocument>(
        `/api/sessions/${encodeURIComponent(name)}`,
        { method: 'POST' },
      )
      setIsCreatingSession(false)
      setNewSessionName('')
      await loadSessions()
      await selectSession(name)
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error)
      setCreateSessionError(message)
    }
  }

  const refresh = async () => {
    try {
      await loadSessions()
      await selectSession(selectedRef.current)
    } catch (error) {
      showError(error)
    }
  }

  const changePermissionMode = (mode: PermissionMode) => {
    setPermissionMode(mode)
    writeLocalPreference('morrow-permission-mode', mode)
  }

  const changeModelSelection = async (selection: ModelSelection) => {
    if (isRunning) return
    const session = selectedRef.current
    const requestId = modelSelectionRequestRef.current + 1
    modelSelectionRequestRef.current = requestId
    const previous = modelSelectionRef.current
    modelSelectionRef.current = selection
    setModelSelection(selection)
    try {
      const response = await fetchJson<SessionModelSelectionResponse>(
        `/api/sessions/${encodeURIComponent(session)}/model-selection`,
        {
          method: 'PUT',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(selection),
        },
      )
      if (
        selectedRef.current !== session ||
        modelSelectionRequestRef.current !== requestId
      ) return
      const saved = response.selection ?? null
      modelSelectionRef.current = saved
      setModelSelection(saved)
    } catch (error) {
      if (
        selectedRef.current !== session ||
        modelSelectionRequestRef.current !== requestId
      ) return
      modelSelectionRef.current = previous
      setModelSelection(previous)
      showError(error)
    }
  }

  const archiveSession = async (name: string) => {
    if (sessionAction) return
    setSessionAction(`archive:${name}`)
    try {
      await fetchJson<SessionArchiveResponse>(
        `/api/sessions/${encodeURIComponent(name)}/archive`,
        { method: 'POST' },
      )
      if (name === selectedRef.current) {
        const latestSessions = await loadSessions()
        let nextName = latestSessions.find((session) => !session.archived)?.name
        if (!nextName) {
          nextName = nextAvailableSessionName(latestSessions)
          await fetchJson<SessionDocument>(
            `/api/sessions/${encodeURIComponent(nextName)}`,
            { method: 'POST' },
          )
        }
        await selectSession(nextName)
      } else {
        await loadSessions()
      }
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
      await fetchJson<SessionArchiveResponse>(
        `/api/sessions/${encodeURIComponent(name)}/restore`,
        { method: 'POST' },
      )
      await selectSession(name)
    } catch (error) {
      showError(error)
    } finally {
      setSessionAction(null)
    }
  }

  const cancelTurn = () => {
    if (!runningTurn || !canCancel) return
    try {
      sendSocketMessage({
        type: 'cancel_turn',
        data: { turn_id: runningTurn.turn_id },
      })
      recordActivity('Cancel requested', runningTurn.turn_id, 'error')
    } catch (error) {
      showError(error)
    }
  }

  const sendApproval = (approved: boolean) => {
    if (!pendingApproval) return
    try {
      sendSocketMessage({
        type: 'approval_decision',
        data: {
          request_id: pendingApproval.id,
          approved,
        },
      })
      setPendingApproval(null)
    } catch (error) {
      showError(error)
    }
  }

  const spawnSubagent = (role: SubagentRole, task: string) => {
    try {
      sendSocketMessage({
        type: 'spawn_subagent',
        data: { request_id: nextId('subagent-request'), role, task },
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
      sendSocketMessage({
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
      sendSocketMessage({
        type: 'inspect_subagent',
        data: { instance_id: instanceId },
      })
    } catch (error) {
      showError(error)
    }
  }

  const cancelSubagent = (instanceId: string) => {
    try {
      sendSocketMessage({
        type: 'cancel_subagent',
        data: { instance_id: instanceId },
      })
    } catch (error) {
      showError(error)
    }
  }

  const deleteSubagent = (instanceId: string) => {
    try {
      sendSocketMessage({
        type: 'delete_subagent',
        data: { instance_id: instanceId },
      })
    } catch (error) {
      showError(error)
    }
  }

  return (
    <SubagentProfilesContext.Provider value={subagentSettings?.profiles ?? []}>
      <>
      <DesktopShell onOpenAbout={() => openSettings('about')}>
        {appView === 'settings' ? (
          <SettingsView
            section={settingsSection}
            status={status}
            theme={themePreference}
            permissionMode={permissionMode}
            modelSettings={modelSettings}
            commandSettings={commandSettings}
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
              modelSelectionRequestRef.current += 1
              await loadModelSettings()
              const selection = await loadSessionModelSelection(selectedRef.current)
              modelSelectionRef.current = selection
              setModelSelection(selection)
            }}
            onCommandSettingsChange={async () => {
              await loadCommandSettings()
            }}
            onSubagentSettingsChange={async () => {
              await loadSubagentSettings()
            }}
          />
        ) : (
          <div
            className={`app-frame${isSidebarOpen ? ' sidebar-open' : ''}${isWorkspaceSidebarCollapsed ? ' sidebar-collapsed' : ''}`}
          >
            <button
              className="mobile-sidebar-backdrop"
              type="button"
              aria-label="Close task navigation"
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
                  setIsDesktopSidebarCollapsed(true)
                }
              }}
              onOpenSettings={openSettings}
              projectsEnabled={desktopPlatform !== null}
              onOpenProjects={() => {
                void refreshDesktopState()
                setWorkspaceDialog('projects')
              }}
              onThemeToggle={() =>
                setThemePreference(resolvedTheme === 'dark' ? 'light' : 'dark')
              }
            />
            <main className="window-main">
              <ChatView
                selected={selected}
                status={status}
                connection={connection}
                timeline={timeline}
                runningTurn={runningTurn}
                pendingApproval={pendingApproval}
                prompt={prompt}
                canSend={canSend}
                canCancel={canCancel}
                isRunning={isRunning}
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
                desktopPlatform={desktopPlatform}
                desktopState={desktopState}
                onOpenLocalWorkspace={openLocalWorkspace}
                onOpenRemoteWorkspace={openRemoteWorkspace}
                onOpenProjects={() => {
                  void refreshDesktopState()
                  setWorkspaceDialog('projects')
                }}
                onReconnectWorkspace={(index) => void reconnectWorkspace(index)}
                onOpenSidebar={openSidebar}
                onOpenInspector={openInspector}
                onToggleRun={(id) => {
                  setTimeline((items) =>
                    items.map((item) =>
                      item.kind === 'run' && item.id === id
                        ? {
                            ...item,
                            trace: {
                              ...item.trace,
                              collapsed: !item.trace.collapsed,
                            },
                          }
                        : item,
                    ),
                  )
                }}
                messageScrollRef={messageScrollRef}
                onMessageScroll={handleMessageScroll}
              />
            </main>
            <InspectorDrawer
              open={isInspectorOpen}
              panel={inspectorPanel}
              tools={tools}
              activity={activity}
              selectedEntry={selectedEntry}
              runningTurn={runningTurn}
              pendingApproval={pendingApproval}
              approvalQueue={approvalQueue}
              subagents={subagents}
              subagentTranscript={subagentTranscript}
              onClose={() => setIsInspectorOpen(false)}
              onPanelChange={setInspectorPanel}
              onSpawnSubagent={spawnSubagent}
              onSendSubagent={sendSubagent}
              onInspectSubagent={inspectSubagent}
              onCancelSubagent={cancelSubagent}
              onDeleteSubagent={deleteSubagent}
            />
          </div>
        )}
      </DesktopShell>
      <ApprovalDialog
        request={pendingApproval}
        onApprove={() => sendApproval(true)}
        onDeny={() => sendApproval(false)}
      />
      {desktopPlatform ? (
        <>
          <ProjectsDialog
            open={workspaceDialog === 'projects'}
            state={desktopState}
            busyIndex={workspaceAction}
            onClose={() => setWorkspaceDialog(null)}
            onOpenLocal={openLocalWorkspace}
            onOpenRemote={openRemoteWorkspace}
            onReconnect={(index) => void reconnectWorkspace(index)}
          />
          <RemoteConnectionDialog
            open={workspaceDialog === 'remote'}
            platform={desktopPlatform}
            onClose={() => setWorkspaceDialog(null)}
          />
        </>
      ) : null}
      </>
    </SubagentProfilesContext.Provider>
  )
}

function AppSidebar({
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
  projectsEnabled,
  onOpenProjects,
  onThemeToggle,
}: {
  sessions: SessionEntryResponse[]
  archivedSessions: SessionEntryResponse[]
  sessionCount: number
  runningTurn: RunningTurnSnapshot | null
  selected: string
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
  projectsEnabled: boolean
  onOpenProjects: () => void
  onThemeToggle: () => void
}) {
  const [isArchiveOpen, setIsArchiveOpen] = useState(false)
  const selectedSessionRef = useRef<HTMLDivElement | null>(null)
  const showArchivedSessions = isArchiveOpen || sessionFilter.trim().length > 0

  useEffect(() => {
    selectedSessionRef.current?.scrollIntoView({ block: 'nearest' })
  }, [selected, sessions])

  return (
    <aside
      id="task-navigation"
      className="app-sidebar workspace-sidebar"
      aria-label="Task navigation"
      aria-hidden={isHidden}
      inert={isHidden}
    >
      <div className="sidebar-brand workspace-brand">
        <strong className="workspace-brand-name">Morrow</strong>
        <MiniIconButton title="Collapse task navigation" onClick={onClose}>
          <PanelLeftClose size={17} />
        </MiniIconButton>
      </div>

      <nav className="sidebar-actions" aria-label="Primary">
        <SidebarAction
          icon={<Plus size={18} />}
          label="New task"
          onClick={onStartCreateSession}
        />
        <SidebarAction
          icon={<Search size={18} />}
          label="Search"
          onClick={onToggleSearch}
        />
        <SidebarAction
          icon={<Folder size={18} />}
          label="Projects"
          badge={projectsEnabled ? undefined : 'Desktop'}
          disabled={!projectsEnabled}
          onClick={onOpenProjects}
        />
        <SidebarAction
          icon={<CalendarClock size={18} />}
          label="Scheduled"
          badge="Soon"
          disabled
        />
        <SidebarAction
          icon={<Plug size={18} />}
          label="Plugins"
          badge="Soon"
          disabled
        />
      </nav>

      <section className="session-browser" aria-label="Tasks">
        <div className="session-browser-head">
          <div>
            <p className="eyebrow">Tasks</p>
            <span>{sessionCount}</span>
          </div>
          <MiniIconButton title="New task" onClick={onStartCreateSession}>
            <Plus size={16} />
          </MiniIconButton>
        </div>

        {isSearchOpen ? (
          <label className="session-search">
            <Search size={16} />
            <input
              ref={searchInputRef}
              value={sessionFilter}
              placeholder="Search tasks"
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
              {sessionFilter.trim() ? 'No matching active tasks.' : 'No active tasks.'}
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
                  onClick={() => onSelectSession(session.name)}
                >
                  <span className="session-name">{session.name}</span>
                  <span>
                    {session.turns} turns
                    {session.has_summary ? ' / summary' : ''}
                  </span>
                </button>
                <button
                  type="button"
                  className="session-row-action archive-action"
                  title={
                    session.path
                      ? `归档任务 ${session.name}`
                      : '空任务无需归档'
                  }
                  aria-label={`归档任务 ${session.name}`}
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
            <section className="archived-session-section" aria-label="Archived tasks">
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
                        <span>{session.turns} turns</span>
                      </div>
                      <button
                        type="button"
                        className="session-row-action restore-action"
                        title={`恢复任务 ${session.name}`}
                        aria-label={`恢复任务 ${session.name}`}
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
            title="Open settings"
            onClick={() => onOpenSettings()}
          >
            <Settings size={17} />
            <span>Settings</span>
          </button>
          <div className="sidebar-footer-actions">
            <MiniIconButton title="Refresh sessions" onClick={onRefresh}>
              <RefreshCw size={16} />
            </MiniIconButton>
            <MiniIconButton title="Toggle theme" onClick={onThemeToggle}>
              {theme === 'dark' ? <Sun size={16} /> : <Moon size={16} />}
            </MiniIconButton>
          </div>
        </div>
      </div>
    </aside>
  )
}

function SidebarAction({
  icon,
  label,
  badge,
  disabled = false,
  onClick,
}: {
  icon: ReactNode
  label: string
  badge?: string
  disabled?: boolean
  onClick?: () => void
}) {
  return (
    <button
      className="sidebar-action"
      type="button"
      title={disabled ? `${label} coming soon` : label}
      disabled={disabled}
      onClick={onClick}
    >
      {icon}
      <span>{label}</span>
      {badge ? <small>{badge}</small> : null}
    </button>
  )
}

function ChatView({
  selected,
  status,
  connection,
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
  desktopPlatform,
  desktopState,
  onOpenLocalWorkspace,
  onOpenRemoteWorkspace,
  onOpenProjects,
  onReconnectWorkspace,
  onOpenSidebar,
  onOpenInspector,
  onToggleRun,
  messageScrollRef,
  onMessageScroll,
}: {
  selected: string
  status: StatusResponse | null
  connection: ConnectionStatus
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
  desktopPlatform: DesktopPlatform | null
  desktopState: DesktopShellState | null
  onOpenLocalWorkspace: () => void
  onOpenRemoteWorkspace: () => void
  onOpenProjects: () => void
  onReconnectWorkspace: (index: number) => void
  onOpenSidebar: () => void
  onOpenInspector: (panel: InspectorPanel) => void
  onToggleRun: (id: string) => void
  messageScrollRef: React.RefObject<HTMLDivElement | null>
  onMessageScroll: (event: UIEvent<HTMLDivElement>) => void
}) {
  const isEmpty = timeline.length === 0
  const composer = (
    <Composer
      prompt={prompt}
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
      desktopPlatform={desktopPlatform}
      desktopState={desktopState}
      onOpenLocalWorkspace={onOpenLocalWorkspace}
      onOpenRemoteWorkspace={onOpenRemoteWorkspace}
      onOpenProjects={onOpenProjects}
      onReconnectWorkspace={onReconnectWorkspace}
    />
  )

  return (
    <section className={`conversation-panel${isEmpty ? ' home-mode' : ''}`}>
      {isEmpty ? (
        <>
          <button
            className="mobile-menu-button home-menu-button"
            type="button"
            aria-label="Open task navigation"
            aria-controls="task-navigation"
            aria-expanded={isSidebarOpen}
            onClick={onOpenSidebar}
          >
            <PanelLeft size={19} />
          </button>
          <HomePrompt status={status}>{composer}</HomePrompt>
        </>
      ) : (
        <>
          <ConversationHeader
            selected={selected}
            connection={connection}
            runningTurn={runningTurn}
            pendingApproval={pendingApproval}
            isSidebarOpen={isSidebarOpen}
            onOpenSidebar={onOpenSidebar}
            onOpenInspector={onOpenInspector}
          />
          <ConversationTimeline
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

function HomePrompt({
  status,
  children,
}: {
  status: StatusResponse | null
  children: ReactNode
}) {
  const workspace = status ? workspaceName(status.workspace_root) : 'this workspace'

  return (
    <div className="home-prompt">
      <div className="home-copy">
        <div className="home-mark" aria-hidden="true">
          <Bot size={29} />
        </div>
        <h1>
          What should we build in <span>{workspace}</span>?
        </h1>
      </div>
      {children}
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
  selected: string
  connection: ConnectionStatus
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
  isSidebarOpen: boolean
  onOpenSidebar: () => void
  onOpenInspector: (panel: InspectorPanel) => void
}) {
  const connectionState = runningTurn ? 'running' : connection

  return (
    <header className="conversation-header">
      <div className="conversation-title">
        <button
          className="mobile-menu-button"
          type="button"
          aria-label="Open task navigation"
          aria-controls="task-navigation"
          aria-expanded={isSidebarOpen}
          onClick={onOpenSidebar}
        >
          <PanelLeft size={19} />
        </button>
        <h1 title={selected}>{selected}</h1>
      </div>
      <div className="conversation-actions">
        <span className={`connection-badge ${connectionState}`}>
          <span />
          {pendingApproval ? 'approval' : connectionState}
        </span>
        <MiniIconButton title="Open run status" onClick={() => onOpenInspector('run')}>
          <Shield size={16} />
        </MiniIconButton>
        <MiniIconButton title="Open subagents" onClick={() => onOpenInspector('subagents')}>
          <Bot size={16} />
        </MiniIconButton>
        <MiniIconButton title="Open tools" onClick={() => onOpenInspector('tools')}>
          <Wrench size={16} />
        </MiniIconButton>
        <MiniIconButton
          title="Open recent activity"
          onClick={() => onOpenInspector('recent')}
        >
          <Clock3 size={16} />
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
}: {
  items: TimelineItem[]
  messageScrollRef: React.RefObject<HTMLDivElement | null>
  onMessageScroll: (event: UIEvent<HTMLDivElement>) => void
  onToggleRun: (id: string) => void
}) {
  return (
    <div
      ref={messageScrollRef}
      className="message-scroll main-scroll"
      onScroll={onMessageScroll}
    >
      <div className="message-column">
        {items.map((item) => {
          if (item.kind === 'message') {
            return <TimelineMessage key={item.id} message={item} />
          }
          if (item.kind === 'run') {
            return (
              <RunTraceCard
                key={item.id}
                trace={item.trace}
                onToggle={() => onToggleRun(item.id)}
              />
            )
          }
          return <TimelineNotice key={item.id} notice={item} />
        })}
      </div>
    </div>
  )
}

function TimelineMessage({ message }: { message: TimelineMessageItem }) {
  return (
    <article className={`message-row ${message.role}`}>
      <div className="message-role">{message.role}</div>
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

function MarkdownContent({
  content,
  className = '',
}: {
  content: string
  className?: string
}) {
  return (
    <div className={`markdown-message${className ? ` ${className}` : ''}`}>
      <ReactMarkdown
        remarkPlugins={markdownPlugins}
        skipHtml
        components={{
          a: ({ node: _node, ...props }) => (
            <a {...props} target="_blank" rel="noreferrer" />
          ),
          table: ({ node: _node, ...props }) => (
            <div className="markdown-table-scroll">
              <table {...props} />
            </div>
          ),
        }}
      >
        {content}
      </ReactMarkdown>
    </div>
  )
}

function TimelineNotice({ notice }: { notice: TimelineNoticeItem }) {
  return (
    <article className={`timeline-notice ${notice.tone}`}>
      {noticeIcon(notice.tone)}
      <div>
        <strong>{notice.title}</strong>
        {notice.detail ? <p>{notice.detail}</p> : null}
      </div>
    </article>
  )
}

function RunTraceCard({
  trace,
  onToggle,
}: {
  trace: RunTrace
  onToggle: () => void
}) {
  const summary = runTraceSummary(trace)
  return (
    <article
      className={`run-card ${trace.status}${trace.collapsed ? ' collapsed' : ' expanded'}`}
    >
      <header className="run-card-head">
        <button
          type="button"
          className="run-toggle"
          title={trace.collapsed ? 'Expand run' : 'Collapse run'}
          aria-expanded={!trace.collapsed}
          onClick={onToggle}
        >
          {trace.collapsed ? (
            <ChevronRight size={18} />
          ) : (
            <ChevronDown size={18} />
          )}
        </button>
        <div className="run-heading">
          <p className="eyebrow">Run</p>
          <h2>{runTraceTitle(trace)}</h2>
          {summary ? <p>{summary}</p> : null}
        </div>
        <div className="run-meta">
          <span>{trace.steps.length} steps</span>
          <span>{trace.toolCount} tools</span>
          <span>{trace.status}</span>
        </div>
      </header>
      {!trace.collapsed ? (
        <div className="run-step-list">
          {trace.steps.map((step) => (
            <RunStepRow key={step.id} step={step} />
          ))}
        </div>
      ) : null}
    </article>
  )
}

function RunStepRow({ step }: { step: RunStep }) {
  const isSubagent = step.kind === 'subagent'
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
              <span>{step.status}</span>
            </div>
            {step.detail ? <p>{step.detail}</p> : null}
            <RunStepDetails step={step} />
          </>
        )}
      </div>
    </article>
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
            `timed out: ${summary.shell.timed_out ? 'yes' : 'no'}`,
            `stdout truncated: ${summary.shell.stdout_truncated ? 'yes' : 'no'}`,
            `stderr truncated: ${summary.shell.stderr_truncated ? 'yes' : 'no'}`,
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
  desktopPlatform,
  desktopState,
  onOpenLocalWorkspace,
  onOpenRemoteWorkspace,
  onOpenProjects,
  onReconnectWorkspace,
}: {
  prompt: string
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
  desktopPlatform: DesktopPlatform | null
  desktopState: DesktopShellState | null
  onOpenLocalWorkspace: () => void
  onOpenRemoteWorkspace: () => void
  onOpenProjects: () => void
  onReconnectWorkspace: (index: number) => void
}) {
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
        (event.key === 'Enter' && !event.metaKey && !event.ctrlKey) ||
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
    ? 'Stop turn'
    : isResolvingCommand
      ? 'Resolving command'
      : 'Send'
  const primaryDisabled = isRunning ? !canCancel : !canSend
  const workspace = status ? workspaceName(status.workspace_root) : 'loading'

  return (
    <form className={`composer ${variant}`} onSubmit={onSubmit}>
      <div className="composer-shell">
        <div className="composer-context" aria-label="Project context">
          {desktopPlatform ? (
            <WorkspaceMenu
              name={workspace}
              path={status?.workspace_root || ''}
              recentWorkspaces={desktopState?.recentWorkspaces ?? []}
              disabled={false}
              onOpenLocal={onOpenLocalWorkspace}
              onOpenRemote={onOpenRemoteWorkspace}
              onOpenProjects={onOpenProjects}
              onReconnect={onReconnectWorkspace}
            />
          ) : (
            <span title={status?.workspace_root || ''}>
              <Folder size={14} />
              {workspace}
            </span>
          )}
        </div>
        <div className="composer-card">
          {commandMenuOpen ? (
            <div className="command-suggestion-menu" role="listbox" aria-label="命令建议">
              <div className="command-suggestion-heading">
                <span>命令</span>
                <small>Enter 选择 · Esc 关闭</small>
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
                    <small>{command.description || '自定义 Markdown 命令'}</small>
                  </span>
                  {command.argument_hint ? <em>{command.argument_hint}</em> : null}
                </button>
              ))}
            </div>
          ) : null}
          <textarea
            value={prompt}
            rows={variant === 'home' ? 3 : 2}
            disabled={isRunning || isResolvingCommand}
            placeholder="Ask Morrow to edit, inspect, or explain this workspace"
            title="Enter 发送·Ctrl + Enter 换行"
            onChange={(event) => {
              setDismissedCommandPrompt('')
              setCommandIndex(0)
              onPromptChange(event.target.value)
            }}
            onKeyDown={handleComposerKeyDown}
          />
          <div className="composer-bar">
            <div className="composer-left">
              <button
                className="composer-chip icon-only"
                type="button"
                title="Attachments coming soon"
                disabled
              >
                <Plus size={16} />
              </button>
              <PermissionPicker
                mode={permissionMode}
                disabled={isRunning || isResolvingCommand}
                onChange={onPermissionModeChange}
              />
            </div>
            <div className="composer-primary">
              <ModelPicker
                settings={modelSettings}
                selection={modelSelection}
                disabled={isRunning || isResolvingCommand}
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
                  <Send size={17} />
                )}
              </button>
            </div>
          </div>
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
  id: PermissionMode | 'plan',
  size = 17,
): ReactNode {
  switch (id) {
    case 'read_only':
      return <Eye size={size} />
    case 'workspace_write':
      return <PencilLine size={size} />
    case 'plan':
      return <ListTree size={size} />
    case 'danger_full_access':
      return <ShieldCheck size={size} />
  }
}

function permissionModeIcon(mode: PermissionMode, size: number): ReactNode {
  return permissionOptionIcon(mode, size)
}

function InspectorDrawer({
  open,
  panel,
  tools,
  activity,
  selectedEntry,
  runningTurn,
  pendingApproval,
  approvalQueue,
  subagents,
  subagentTranscript,
  onClose,
  onPanelChange,
  onSpawnSubagent,
  onSendSubagent,
  onInspectSubagent,
  onCancelSubagent,
  onDeleteSubagent,
}: {
  open: boolean
  panel: InspectorPanel
  tools: ToolRun[]
  activity: ActivityItem[]
  selectedEntry?: SessionEntryResponse
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
  approvalQueue: ApprovalRequest[]
  subagents: SubagentInstanceSnapshot[]
  subagentTranscript: SubagentTranscriptSnapshot | null
  onClose: () => void
  onPanelChange: (panel: InspectorPanel) => void
  onSpawnSubagent: (role: SubagentRole, task: string) => void
  onSendSubagent: (instanceId: string, message: string) => void
  onInspectSubagent: (instanceId: string) => void
  onCancelSubagent: (instanceId: string) => void
  onDeleteSubagent: (instanceId: string) => void
}) {
  return (
    <aside
      className={`inspector-drawer${open ? ' open' : ''}`}
      aria-hidden={!open}
      inert={!open}
    >
      <button
        className="drawer-backdrop"
        type="button"
        aria-label="Close inspector"
        onClick={onClose}
      />
      <section className="drawer-panel main-scroll" aria-label="Inspector">
        <header className="drawer-header">
          <div>
            <p className="eyebrow">Inspector</p>
            <h2>{inspectorPanelTitle(panel)}</h2>
          </div>
          <MiniIconButton title="Close inspector" onClick={onClose}>
            <X size={18} />
          </MiniIconButton>
        </header>
        <nav className="drawer-tabs" aria-label="Inspector panels">
          <DrawerTab
            active={panel === 'run'}
            icon={<Shield size={16} />}
            label="Run"
            onClick={() => onPanelChange('run')}
          />
          <DrawerTab
            active={panel === 'subagents'}
            icon={<Bot size={16} />}
            label="Agents"
            onClick={() => onPanelChange('subagents')}
          />
          <DrawerTab
            active={panel === 'tools'}
            icon={<Wrench size={16} />}
            label="Tools"
            onClick={() => onPanelChange('tools')}
          />
          <DrawerTab
            active={panel === 'recent'}
            icon={<Clock3 size={16} />}
            label="Recent"
            onClick={() => onPanelChange('recent')}
          />
        </nav>
        <InspectorPanelContent
          panel={panel}
          tools={tools}
          activity={activity}
          selectedEntry={selectedEntry}
          runningTurn={runningTurn}
          pendingApproval={pendingApproval}
          approvalQueue={approvalQueue}
          subagents={subagents}
          subagentTranscript={subagentTranscript}
          onSpawnSubagent={onSpawnSubagent}
          onSendSubagent={onSendSubagent}
          onInspectSubagent={onInspectSubagent}
          onCancelSubagent={onCancelSubagent}
          onDeleteSubagent={onDeleteSubagent}
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
  tools,
  activity,
  selectedEntry,
  runningTurn,
  pendingApproval,
  approvalQueue,
  subagents,
  subagentTranscript,
  onSpawnSubagent,
  onSendSubagent,
  onInspectSubagent,
  onCancelSubagent,
  onDeleteSubagent,
}: {
  panel: InspectorPanel
  tools: ToolRun[]
  activity: ActivityItem[]
  selectedEntry?: SessionEntryResponse
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
  approvalQueue: ApprovalRequest[]
  subagents: SubagentInstanceSnapshot[]
  subagentTranscript: SubagentTranscriptSnapshot | null
  onSpawnSubagent: (role: SubagentRole, task: string) => void
  onSendSubagent: (instanceId: string, message: string) => void
  onInspectSubagent: (instanceId: string) => void
  onCancelSubagent: (instanceId: string) => void
  onDeleteSubagent: (instanceId: string) => void
}) {
  if (panel === 'subagents') {
    return (
      <PersistentSubagentPanel
        instances={subagents}
        transcript={subagentTranscript}
        onSpawn={onSpawnSubagent}
        onSend={onSendSubagent}
        onInspect={onInspectSubagent}
        onCancel={onCancelSubagent}
        onDelete={onDeleteSubagent}
      />
    )
  }

  if (panel === 'tools') {
    return <ToolList tools={tools} />
  }

  if (panel === 'recent') {
    return <ActivityList items={[...activity].reverse()} />
  }

  return (
    <div className="drawer-run">
      <div className="inspector-metrics">
        <InspectorMetric label="turns" value={String(selectedEntry?.turns ?? 0)} />
        <InspectorMetric
          label="active"
          value={String(selectedEntry?.active_messages ?? 0)}
        />
        <InspectorMetric
          label="summary"
          value={selectedEntry?.has_summary ? 'yes' : 'no'}
        />
      </div>
      <div className="status-card">
        <p className="eyebrow">Turn</p>
        <strong>{pendingApproval ? 'approval' : runningTurn ? 'running' : 'idle'}</strong>
        {runningTurn ? <small>{runningTurn.turn_id}</small> : null}
        {pendingApproval ? (
          <span className="notice-pill approval">approval pending</span>
        ) : null}
      </div>
      {approvalQueue.length > 0 ? (
        <div className="approval-queue-card">
          <p className="eyebrow">Approval queue</p>
          {approvalQueue.map((request, index) => (
            <span key={request.id}>
              <strong>{index === 0 ? 'Current' : `Queued ${index}`}</strong>
              <small>{approvalSource(request)}</small>
            </span>
          ))}
        </div>
      ) : null}
      <div className="panel-title compact">
        <Wrench size={18} />
        <span>Tools</span>
      </div>
      <ToolList tools={tools} compact />
      <div className="panel-title compact">
        <Clock3 size={18} />
        <span>Recent</span>
      </div>
      <ActivityList items={activity.slice(-5).reverse()} compact />
    </div>
  )
}

function InspectorMetric({ label, value }: { label: string; value: string }) {
  return (
    <span className="inspector-metric">
      <strong>{value}</strong>
      <span>{label}</span>
    </span>
  )
}

export function PersistentSubagentPanel({
  instances,
  transcript,
  onSpawn,
  onSend,
  onInspect,
  onCancel,
  onDelete,
}: {
  instances: SubagentInstanceSnapshot[]
  transcript: SubagentTranscriptSnapshot | null
  onSpawn: (role: SubagentRole, task: string) => void
  onSend: (instanceId: string, message: string) => void
  onInspect: (instanceId: string) => void
  onCancel: (instanceId: string) => void
  onDelete: (instanceId: string) => void
}) {
  const profiles = useContext(SubagentProfilesContext)
  const [role, setRole] = useState<SubagentRole>('explore')
  const [task, setTask] = useState('')
  const [followup, setFollowup] = useState('')
  const [showEvents, setShowEvents] = useState(false)
  const selected = transcript?.instance
  useEffect(() => setShowEvents(false), [transcript?.instance.id])
  const spawn = (event: FormEvent) => {
    event.preventDefault()
    const value = task.trim()
    if (!value) return
    onSpawn(role, value)
    setTask('')
  }
  const send = (event: FormEvent) => {
    event.preventDefault()
    const value = followup.trim()
    if (!selected || !value) return
    onSend(selected.id, value)
    setFollowup('')
  }

  return (
    <div className="persistent-subagent-panel">
      <form className="subagent-spawn-form" onSubmit={spawn}>
        <div className="panel-title compact">
          <Plus size={17} />
          <span>New instance</span>
        </div>
        <select value={role} onChange={(event) => setRole(event.target.value as SubagentRole)}>
          <option value="explore">Explore · read-only research</option>
          <option value="plan">Plan · read-only planning</option>
          <option value="worker">Worker · workspace changes</option>
          <option value="reviewer">Reviewer · review + approved shell</option>
        </select>
        <textarea
          value={task}
          maxLength={4000}
          placeholder="Give this instance a self-contained task…"
          onChange={(event) => setTask(event.target.value)}
        />
        <button className="approve-button" type="submit" disabled={!task.trim()}>
          <Send size={15} /> Start in background
        </button>
      </form>

      <div className="subagent-instance-list">
        {instances.length === 0 ? <p className="muted-line">No persistent subagents.</p> : null}
        {instances.map((instance) => {
          const profile = findSubagentProfile(
            profiles,
            instance.identity.id,
            instance.identity.name,
          )
          return (
            <button
              className={`subagent-instance-card${selected?.id === instance.id ? ' selected' : ''}`}
              type="button"
              key={instance.id}
              onClick={() => onInspect(instance.id)}
            >
              <span className="subagent-instance-avatar">
                <SubagentIdentityAvatar avatar={profile?.avatar_data_url} />
              </span>
              <span>
                <strong>{instance.identity.name}</strong>
                <small>{compactText(instance.latest_task ?? 'No task', 72)}</small>
              </span>
              <span className={`subagent-status-badge ${instance.status}`}>
                {instance.role} · {instance.status.replace('_', ' ')}
              </span>
              {instance.queue_reason ? <em>{instance.queue_reason}</em> : null}
            </button>
          )
        })}
      </div>

      {transcript ? (
        <section className="subagent-transcript">
          <header>
            <div>
              <p className="eyebrow">Instance transcript</p>
              <h3>{transcript.instance.identity.name}</h3>
              <small>
                {transcript.model.provider_name} / {transcript.model.model_name} · {transcript.model.reasoning}
              </small>
            </div>
            <span className={`subagent-status-badge ${transcript.instance.status}`}>
              {transcript.instance.status.replace('_', ' ')}
            </span>
          </header>
          <div className="subagent-runtime-meta">
            <span>{transcript.instance.role}</span>
            <span>{transcript.permission_ceiling.mode}</span>
            <span>shell {transcript.permission_ceiling.shell}</span>
            <span>{transcript.role_config.timeout_secs}s</span>
            <span>{transcript.role_config.max_tool_rounds} rounds</span>
          </div>
          {transcript.instance.event_log_truncated ? (
            <p className="subagent-log-warning">Streaming deltas were truncated after the 16 MiB log budget.</p>
          ) : null}
          <div className="subagent-run-list">
            {transcript.runs.map((run) => (
              <article key={run.id}>
                <strong>{run.status}</strong>
                <small>{new Date(run.started_at_ms).toLocaleString()}</small>
                <p>{run.task}</p>
                {run.summary?.result ? <pre>{run.summary.result}</pre> : null}
                {run.summary?.error ? <pre className="error-copy">{run.summary.error}</pre> : null}
              </article>
            ))}
          </div>
          <div className="subagent-message-transcript">
            {subagentTranscriptMessages(transcript.session).map((message, index) => (
              <article className={`subagent-message ${message.role}`} key={`${message.role}-${index}`}>
                <strong>{message.role}</strong>
                {message.reasoning_content ? <pre>{message.reasoning_content}</pre> : null}
                {message.content ? <pre>{message.content}</pre> : null}
                {message.tool_calls?.map((call) => (
                  <pre key={call.id}>{call.function.name} {call.function.arguments}</pre>
                ))}
              </article>
            ))}
          </div>
          <div className="subagent-event-log-controls">
            <button
              className="secondary-button"
              type="button"
              onClick={() => setShowEvents((current) => !current)}
            >
              <ListTree size={14} /> {showEvents ? 'Hide' : 'Show'} event log ({transcript.events.length})
            </button>
          </div>
          {showEvents ? (
            <pre className="subagent-event-log">
              {transcript.events.map((event) => JSON.stringify(event)).join('\n')}
            </pre>
          ) : null}
          <div className="subagent-instance-actions">
            {isActiveSubagentStatus(transcript.instance.status) ? (
              <button className="danger-button subtle" type="button" onClick={() => onCancel(transcript.instance.id)}>
                <Square size={14} /> Cancel
              </button>
            ) : (
              <button
                className="danger-button subtle"
                type="button"
                onClick={() => {
                  if (window.confirm(`Delete ${transcript.instance.identity.name} and its transcript?`)) {
                    onDelete(transcript.instance.id)
                  }
                }}
              >
                <X size={14} /> Delete
              </button>
            )}
          </div>
          {!isActiveSubagentStatus(transcript.instance.status) ? (
            <form className="subagent-followup-form" onSubmit={send}>
              <textarea
                value={followup}
                maxLength={4000}
                placeholder="Continue this instance with preserved context…"
                onChange={(event) => setFollowup(event.target.value)}
              />
              <button className="approve-button" type="submit" disabled={!followup.trim()}>
                <Send size={14} /> Continue
              </button>
            </form>
          ) : null}
        </section>
      ) : null}
    </div>
  )
}

export function subagentTranscriptMessages(session: Session): Message[] {
  if (session.turns.length > 0) {
    return session.turns.flatMap((record) => record.messages)
  }
  return session.active_thread.messages
}

function isActiveSubagentStatus(status: SubagentInstanceSnapshot['status']): boolean {
  return status === 'queued' || status === 'running' || status === 'waiting_approval'
}

function ToolList({
  tools,
  compact = false,
}: {
  tools: ToolRun[]
  compact?: boolean
}) {
  if (tools.length === 0) {
    return <p className="muted-line">No tool calls.</p>
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

function ActivityList({
  items,
  compact = false,
}: {
  items: ActivityItem[]
  compact?: boolean
}) {
  if (items.length === 0) {
    return <p className="muted-line">No events.</p>
  }

  return (
    <div className={`activity-list${compact ? ' compact' : ''}`}>
      {items.map((item) => (
        <article key={item.id} className={`activity-item ${item.tone}`}>
          <span>{item.time}</span>
          <div>
            <strong>{item.title}</strong>
            {item.detail ? <p>{item.detail}</p> : null}
          </div>
        </article>
      ))}
    </div>
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
        aria-label="New session name"
        autoFocus
        value={value}
        placeholder="session name"
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={handleKeyDown}
      />
      <div className="session-create-actions">
        <MiniIconButton title="Create session" type="submit" disabled={!canSubmit}>
          <Check size={17} />
        </MiniIconButton>
        <MiniIconButton title="Cancel" onClick={onCancel}>
          <X size={17} />
        </MiniIconButton>
      </div>
      {error ? <p>{error}</p> : null}
    </form>
  )
}

function ApprovalDialog({
  request,
  onApprove,
  onDeny,
}: {
  request: ApprovalRequest | null
  onApprove: () => void
  onDeny: () => void
}) {
  if (!request) return null

  return (
    <div className="approval-overlay" role="dialog" aria-modal="true">
      <section className="approval-panel">
        <header>
          <div>
            <p className="eyebrow">Approval</p>
            <h2>{approvalTitle(request)}</h2>
          </div>
          <IconButton title="Deny" onClick={onDeny}>
            <X size={20} />
          </IconButton>
        </header>
        <p className="approval-source">Source: {approvalSource(request)}</p>
        <p className="approval-reason">{request.reason}</p>
        <ApprovalBody request={request} />
        <footer>
          <button className="danger-button" type="button" onClick={onDeny}>
            Deny
          </button>
          <button className="approve-button" type="button" onClick={onApprove}>
            <CheckCircle2 size={18} />
            <span>Approve</span>
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

function IconButton({
  title,
  disabled = false,
  onClick,
  children,
}: {
  title: string
  disabled?: boolean
  onClick: () => void
  children: ReactNode
}) {
  return (
    <button
      className="icon-button"
      type="button"
      title={title}
      disabled={disabled}
      onClick={onClick}
    >
      <span className="sr-only">{title}</span>
      {children}
    </button>
  )
}

function MiniIconButton({
  title,
  type = 'button',
  disabled = false,
  onClick,
  children,
}: {
  title: string
  type?: 'button' | 'submit'
  disabled?: boolean
  onClick?: () => void
  children: ReactNode
}) {
  return (
    <button
      className="mini-icon-button"
      type={type}
      title={title}
      disabled={disabled}
      onClick={onClick}
    >
      <span className="sr-only">{title}</span>
      {children}
    </button>
  )
}

function sessionTimeline(session: Session): TimelineItem[] {
  if (session.turns.length > 0) {
    return session.turns.flatMap((record, index) =>
      turnRecordTimeline(record, index),
    )
  }

  return session.active_thread.messages.flatMap((message, index) =>
    fallbackMessageTimeline(message, index),
  )
}

function turnRecordTimeline(
  record: Session['turns'][number],
  turnIndex: number,
): TimelineItem[] {
  const items: TimelineItem[] = []
  const userContent = record.turn.user_message.content
  if (userContent) {
    items.push({
      kind: 'message',
      id: `history-${turnIndex}-user`,
      role: 'user',
      content: userContent,
    })
  }

  if (record.turn.steps.length > 0 || record.turn.error) {
    const trace = historyRunTrace(record, turnIndex)
    items.push({
      kind: 'run',
      id: trace.id,
      trace,
    })
  }

  const assistantContent = finalAssistantContent(record)
  if (assistantContent) {
    items.push({
      kind: 'message',
      id: `history-${turnIndex}-assistant`,
      role: 'assistant',
      content: assistantContent,
    })
  }

  return items
}

function historyRunTrace(
  record: Session['turns'][number],
  turnIndex: number,
): RunTrace {
  const turn = record.turn
  const subagents = subagentHistory(record.messages)
  const modelMessages = record.messages.filter(
    (message) => message.role === 'assistant',
  )
  let modelMessageIndex = 0
  const steps: RunStep[] = turn.steps.map((step, stepIndex) => {
    const isSubagent =
      step.kind === 'tool_call' && step.tool_name === 'delegate_task'
    const subagent = step.tool_call_id
      ? subagents.get(step.tool_call_id)
      : undefined
    const modelMessage =
      step.kind === 'model_call'
        ? modelMessages[modelMessageIndex++]
        : undefined
    return {
      id: `history-${turnIndex}-step-${stepIndex}`,
      kind: isSubagent
        ? 'subagent'
        : step.kind === 'tool_call'
          ? 'tool'
          : 'model',
      status:
        step.status === 'completed'
          ? 'ok'
          : step.status === 'failed'
            ? 'error'
            : 'running',
      title:
        isSubagent
          ? subagentStepTitle(subagent?.agentName)
          : step.kind === 'tool_call'
          ? step.tool_name || 'Tool call'
          : turn.model?.model_name || 'Model call',
      detail:
        (isSubagent && subagent?.task) ||
        step.error ||
        step.tool_call_id ||
        (turn.model
          ? `${turn.model.provider_name} · ${reasoningLabel(turn.model.reasoning)}`
          : undefined),
      reasoning: modelMessage?.reasoning_content || undefined,
      summary: subagent?.summary
        ? { subagent: subagent.summary }
        : undefined,
      agentId: subagent?.agentId,
      agentName: subagent?.agentName,
    }
  })

  if (turn.error && !steps.some((step) => step.status === 'error')) {
    steps.push({
      id: `history-${turnIndex}-error`,
      kind: 'error',
      status: 'error',
      title: 'Error',
      detail: turn.error,
    })
  }

  return {
    id: `history-${turnIndex}-run`,
    status:
      turn.status === 'completed'
        ? 'completed'
        : turn.status === 'failed'
          ? 'failed'
          : 'running',
    collapsed: true,
    startedAt: `turn ${turnIndex + 1}`,
    steps,
    toolCount: steps.filter((step) => step.kind === 'tool').length,
  }
}

function finalAssistantContent(record: Session['turns'][number]): string {
  const direct = record.turn.assistant_message?.content
  if (direct?.trim()) return direct

  const fallback = [...record.messages]
    .reverse()
    .find(
      (message) =>
        message.role === 'assistant' &&
        Boolean(message.content?.trim()) &&
        !message.tool_calls?.length,
    )
  return fallback?.content || ''
}

function fallbackMessageTimeline(
  message: Message,
  index: number,
): TimelineItem[] {
  if (message.role === 'system') return []

  const content = message.content ?? formatToolCalls(message)
  if (!content) return []

  if (message.role === 'tool') {
    return [
      {
        kind: 'notice',
        id: `history-${index}-tool`,
        tone: 'neutral',
        title: 'Tool result',
        detail: compactText(content, 180),
      },
    ]
  }

  if (message.role === 'assistant' && message.tool_calls?.length && !message.content) {
    return [
      {
        kind: 'notice',
        id: `history-${index}-tool-calls`,
        tone: 'neutral',
        title: 'Tool calls',
        detail: compactText(content, 180),
      },
    ]
  }

  return [
    {
      kind: 'message',
      id: `history-${index}-${message.role}`,
      role: message.role,
      content,
    },
  ]
}

function formatToolCalls(message: Message): string {
  if (message.tool_calls) {
    return JSON.stringify(message.tool_calls, null, 2)
  }
  if (message.tool_call_id) {
    return `tool_call_id: ${message.tool_call_id}`
  }
  return ''
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

function runTraceTitle(trace: RunTrace): string {
  switch (trace.status) {
    case 'approval':
      return 'Waiting for approval'
    case 'completed':
      return 'Execution complete'
    case 'failed':
      return 'Execution failed'
    case 'running':
      return 'Executing task'
  }
}

function runTraceSummary(trace: RunTrace): string {
  const lastStep = trace.steps.at(-1)
  if (!lastStep) return trace.completedAt || trace.startedAt
  const detail = lastStep.detail ? ` - ${compactText(lastStep.detail, 90)}` : ''
  return `${lastStep.title}${detail}`
}

function noticeIcon(tone: TimelineNoticeItem['tone']): ReactNode {
  switch (tone) {
    case 'running':
      return <Clock3 size={18} />
    case 'ok':
      return <CheckCircle2 size={18} />
    case 'error':
      return <CircleAlert size={18} />
    case 'approval':
      return <Shield size={18} />
    case 'neutral':
      return <Activity size={18} />
  }
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
    case 'tool':
      return step.summary?.files?.length ? (
        <FileText size={18} />
      ) : (
        <Terminal size={18} />
      )
  }
}

function currentTime(): string {
  return new Date().toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  })
}

function inspectorPanelTitle(panel: InspectorPanel): string {
  switch (panel) {
    case 'run':
      return 'Run'
    case 'subagents':
      return 'Subagents'
    case 'tools':
      return 'Tools'
    case 'recent':
      return 'Recent'
  }
}

function approvalSource(request: ApprovalRequest): string {
  const origin = request.origin
  if (!origin || origin.kind === 'unknown') return 'parent agent'
  if (origin.kind === 'parent_turn') return origin.turn_id ?? 'parent turn'
  return `${origin.identity_name ?? origin.role} · ${origin.role} · ${origin.run_id}`
}

function approvalTitle(request: ApprovalRequest): string {
  return request.action.kind === 'shell_command'
    ? 'Shell command'
    : 'File changes'
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
): boolean {
  return key === 'Enter' && !ctrlKey && !isComposing
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
  session: string
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
      requestedSection === 'commands')
      ? requestedSection
      : 'general'

  return {
    session: params.get('session') || 'default',
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
  session: string,
  view: AppView,
  section: SettingsSection,
  method: 'push' | 'replace',
  fromWorkspace: boolean,
): void {
  const params = new URLSearchParams()
  params.set('session', session)
  if (view === 'settings') {
    params.set('view', 'settings')
    params.set('section', section)
  }

  const url = `${location.pathname}?${params.toString()}`
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

function nextAvailableSessionName(sessions: SessionEntryResponse[]): string {
  const names = new Set(sessions.map((session) => session.name))
  if (!names.has('default')) return 'default'

  let index = 1
  while (names.has(`task-${index}`)) index += 1
  return `task-${index}`
}
