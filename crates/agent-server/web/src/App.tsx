import type { FormEvent, KeyboardEvent, ReactNode } from 'react'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  Activity,
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
  ListTree,
  Moon,
  PanelLeft,
  Plug,
  Plus,
  RefreshCw,
  Search,
  Send,
  Settings,
  Shield,
  Square,
  Sun,
  Terminal,
  Wrench,
  X,
} from 'lucide-react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { fetchJson, sessionSocketUrl } from './api'
import type {
  ActivityItem,
  AgentEvent,
  ApprovalRequest,
  ClientMessage,
  Message,
  RunningTurnSnapshot,
  ServerMessage,
  Session,
  SessionDocument,
  SessionEntryResponse,
  StatusResponse,
  TimelineItem,
  TimelineMessageItem,
  TimelineNoticeItem,
  RunStep,
  RunTrace,
  ToolExecutionSummary,
  ToolRun,
} from './types'

type InspectorPanel = 'run' | 'tools' | 'recent'
type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'

const markdownPlugins = [remarkGfm]

const emptySessionEntry = (name: string): SessionEntryResponse => ({
  name,
  path: '',
  turns: 0,
  active_messages: 0,
  summarized_turns: 0,
  has_summary: false,
})

export default function App() {
  const [status, setStatus] = useState<StatusResponse | null>(null)
  const [sessions, setSessions] = useState<SessionEntryResponse[]>([])
  const [selected, setSelected] = useState('default')
  const [timeline, setTimeline] = useState<TimelineItem[]>([])
  const [tools, setTools] = useState<ToolRun[]>([])
  const [activity, setActivity] = useState<ActivityItem[]>([])
  const [sessionFilter, setSessionFilter] = useState('')
  const [isSearchOpen, setIsSearchOpen] = useState(false)
  const [isSidebarOpen, setIsSidebarOpen] = useState(false)
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
  const [connection, setConnection] =
    useState<ConnectionStatus>('disconnected')
  const [prompt, setPrompt] = useState('')
  const [theme, setTheme] = useState<'light' | 'dark'>(() =>
    localStorage.getItem('morrow-theme') === 'dark' ? 'dark' : 'light',
  )

  const socketRef = useRef<WebSocket | null>(null)
  const selectedRef = useRef(selected)
  const assistantMessageIdRef = useRef<string | null>(null)
  const runTraceIdRef = useRef<string | null>(null)
  const idRef = useRef(0)
  const selectionRef = useRef(0)
  const messagesEndRef = useRef<HTMLDivElement | null>(null)
  const sessionSearchRef = useRef<HTMLInputElement | null>(null)

  const nextId = useCallback((prefix: string) => {
    idRef.current += 1
    return `${prefix}-${Date.now()}-${idRef.current}`
  }, [])

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
    (title = 'Model call started', detail?: string) => {
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
      const runId = ensureRunTrace('Model requested a tool', name)
      updateRunTrace(runId, (trace) => ({
        ...trace,
        steps: trace.steps.map((step) =>
          step.kind === 'model' && step.status === 'running'
            ? { ...step, status: 'ok' }
            : step,
        ),
      }))
      upsertRunStep(runId, {
        id,
        kind: 'tool',
        status: 'running',
        title: name,
        detail: 'Tool call started',
      })
    },
    [ensureRunTrace, updateRunTrace, upsertRunStep],
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
    [nextId],
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
    setSessions(
      entries.some((session) => session.name === current)
        ? entries
        : [emptySessionEntry(current), ...entries],
    )
  }, [])

  const sendSocketMessage = useCallback((message: ClientMessage) => {
    const socket = socketRef.current
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      throw new Error('websocket is not connected')
    }
    socket.send(JSON.stringify(message))
  }, [])

  const handleAgentEvent = useCallback(
    (event: AgentEvent) => {
      switch (event.type) {
        case 'turn_started':
          assistantMessageIdRef.current = null
          setTools([])
          refreshCurrentModelStep('Model call started', selectedRef.current)
          recordActivity('Turn started', selectedRef.current, 'running')
          break
        case 'warning':
          recordActivity('Warning', event.data, 'error')
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
          setPendingApproval(event.data)
          setApprovalStep(event.data.id, 'Approval requested', event.data.reason)
          recordActivity('Approval requested', event.data.reason, 'approval')
          break
        case 'approval_resolved':
          setPendingApproval(null)
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
      appendAssistantDelta,
      completeCurrentRun,
      finishToolStep,
      recordActivity,
      refreshCurrentModelStep,
      setApprovalStep,
      showError,
      startToolStep,
      upsertTool,
    ],
  )

  const handleServerMessage = useCallback(
    (message: ServerMessage) => {
      switch (message.type) {
        case 'snapshot':
          setRunningTurn(message.data.running_turn ?? null)
          break
        case 'agent_event':
          handleAgentEvent(message.data.event)
          break
        case 'turn_saved':
          void loadSessions().catch(showError)
          setRunningTurn(null)
          recordActivity('Turn saved', `#${message.data.turn_index}`, 'ok')
          break
        case 'turn_rejected':
          setRunningTurn(null)
          showError(message.data.reason)
          break
        case 'error':
          setRunningTurn(null)
          showError(message.data.message)
          break
      }
    },
    [handleAgentEvent, loadSessions, recordActivity, showError],
  )

  const closeSocket = useCallback(() => {
    const socket = socketRef.current
    if (!socket) return
    socket.onclose = null
    socket.close()
    socketRef.current = null
  }, [])

  const openSocket = useCallback(
    (name: string) => {
      const socket = new WebSocket(sessionSocketUrl(name))
      socketRef.current = socket
      setConnection('connecting')

      socket.addEventListener('open', () => {
        if (socketRef.current !== socket) return
        setConnection('connected')
        recordActivity('Socket connected', name, 'ok')
      })

      socket.addEventListener('close', () => {
        if (socketRef.current !== socket) return
        setConnection('disconnected')
        setRunningTurn(null)
        recordActivity('Socket disconnected', name, 'neutral')
      })

      socket.addEventListener('message', (event) => {
        try {
          handleServerMessage(JSON.parse(event.data) as ServerMessage)
        } catch (error) {
          showError(error)
        }
      })
    },
    [handleServerMessage, recordActivity, showError],
  )

  const selectSession = useCallback(
    async (name: string) => {
      const selectionId = selectionRef.current + 1
      selectionRef.current = selectionId
      selectedRef.current = name
      setSelected(name)
      setIsSidebarOpen(false)
      setRunningTurn(null)
      setPendingApproval(null)
      setTools([])
      setActivity([])
      assistantMessageIdRef.current = null
      runTraceIdRef.current = null
      setTimeline([])
      closeSocket()
      history.replaceState(null, '', `?session=${encodeURIComponent(name)}`)

      try {
        const document = await fetchJson<SessionDocument>(
          `/api/sessions/${encodeURIComponent(name)}`,
        )
        if (selectionRef.current !== selectionId) return
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
    [closeSocket, loadSessions, openSocket, recordActivity, showError],
  )

  useEffect(() => {
    selectedRef.current = selected
  }, [selected])

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
    document.documentElement.classList.toggle('dark', theme === 'dark')
    localStorage.setItem('morrow-theme', theme)
  }, [theme])

  useEffect(() => {
    const media = window.matchMedia('(max-width: 900px)')
    const handleChange = (event: MediaQueryListEvent) => {
      setIsNarrowViewport(event.matches)
      if (!event.matches) setIsSidebarOpen(false)
    }

    media.addEventListener('change', handleChange)
    return () => media.removeEventListener('change', handleChange)
  }, [])

  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ block: 'end' })
  }, [timeline])

  useEffect(() => {
    let mounted = true
    async function boot() {
      try {
        const loadedStatus = await fetchJson<StatusResponse>('/api/status')
        if (!mounted) return
        setStatus(loadedStatus)
        const name = new URLSearchParams(location.search).get('session') || 'default'
        selectedRef.current = name
        await loadSessions()
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
  }, [closeSocket, loadSessions, selectSession, showError])

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

  const isRunning = Boolean(runningTurn)
  const canSend = connection === 'connected' && !isRunning && prompt.trim().length > 0
  const canCancel = Boolean(runningTurn?.turn_id && runningTurn.turn_id !== 'pending')

  const openInspector = (panel: InspectorPanel) => {
    setIsSidebarOpen(false)
    setInspectorPanel(panel)
    setIsInspectorOpen(true)
  }

  const openSidebar = () => {
    setIsInspectorOpen(false)
    setIsSidebarOpen(true)
  }

  const toggleSearch = () => {
    if (isSearchOpen) setSessionFilter('')
    setIsSearchOpen((open) => !open)
  }

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    const trimmed = prompt.trim()
    if (!trimmed || !canSend) return
    try {
      addTimelineMessage('user', trimmed)
      createRunTrace('Request queued', compactText(trimmed, 90))
      sendSocketMessage({
        type: 'start_turn',
        data: {
          request_id: `request-${Date.now()}`,
          prompt: trimmed,
        },
      })
      setPrompt('')
      setRunningTurn({ turn_id: 'pending' })
      recordActivity('Turn requested', compactText(trimmed, 90), 'running')
    } catch (error) {
      showError(error)
    }
  }

  const handlePromptKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
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

  return (
    <>
      <div className={`app-frame${isSidebarOpen ? ' sidebar-open' : ''}`}>
        <button
          className="mobile-sidebar-backdrop"
          type="button"
          aria-label="Close task navigation"
          aria-hidden={!isSidebarOpen}
          tabIndex={isSidebarOpen ? 0 : -1}
          onClick={() => setIsSidebarOpen(false)}
        />
        <AppSidebar
          sessions={filteredSessions}
          sessionCount={sessions.length}
          status={status}
          connection={connection}
          runningTurn={runningTurn}
          selected={selected}
          isCreatingSession={isCreatingSession}
          newSessionName={newSessionName}
          createSessionError={createSessionError}
          isSearchOpen={isSearchOpen}
          sessionFilter={sessionFilter}
          theme={theme}
          searchInputRef={sessionSearchRef}
          isHidden={isNarrowViewport && !isSidebarOpen}
          onSelectSession={(name) => void selectSession(name)}
          onStartCreateSession={startCreateSession}
          onCancelCreateSession={cancelCreateSession}
          onNewSessionNameChange={setNewSessionName}
          onCreateSession={() => void createSession()}
          onToggleSearch={toggleSearch}
          onSessionFilterChange={setSessionFilter}
          onRefresh={() => void refresh()}
          onClose={() => setIsSidebarOpen(false)}
          onThemeToggle={() =>
            setTheme((current) => (current === 'dark' ? 'light' : 'dark'))
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
            isSidebarOpen={isSidebarOpen}
            onPromptChange={setPrompt}
            onPromptKeyDown={handlePromptKeyDown}
            onSubmit={handleSubmit}
            onCancel={cancelTurn}
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
            messagesEndRef={messagesEndRef}
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
          onClose={() => setIsInspectorOpen(false)}
          onPanelChange={setInspectorPanel}
        />
      </div>
      <ApprovalDialog
        request={pendingApproval}
        onApprove={() => sendApproval(true)}
        onDeny={() => sendApproval(false)}
      />
    </>
  )
}

function AppSidebar({
  sessions,
  sessionCount,
  status,
  connection,
  runningTurn,
  selected,
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
  onRefresh,
  onClose,
  onThemeToggle,
}: {
  sessions: SessionEntryResponse[]
  sessionCount: number
  status: StatusResponse | null
  connection: ConnectionStatus
  runningTurn: RunningTurnSnapshot | null
  selected: string
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
  onRefresh: () => void
  onClose: () => void
  onThemeToggle: () => void
}) {
  const workspace = status ? workspaceName(status.workspace_root) : 'loading'
  const permission = status
    ? formatPermissionMode(status.permissions.mode)
    : 'unknown mode'
  const connectionState = runningTurn ? 'running' : connection
  const selectedSessionRef = useRef<HTMLButtonElement | null>(null)

  useEffect(() => {
    selectedSessionRef.current?.scrollIntoView({ block: 'nearest' })
  }, [selected, sessions])

  return (
    <aside
      id="task-navigation"
      className="app-sidebar"
      aria-label="Task navigation"
      aria-hidden={isHidden}
      inert={isHidden}
    >
      <div className="sidebar-brand">
        <div className="brand-mark">M</div>
        <div className="sidebar-brand-copy">
          <strong>Morrow</strong>
          <span>Agent workspace</span>
        </div>
        <MiniIconButton title="Close task navigation" onClick={onClose}>
          <X size={17} />
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
          badge="Soon"
          disabled
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
              {sessionFilter.trim() ? 'No matching sessions.' : 'No sessions.'}
            </p>
          ) : (
            sessions.map((session) => (
              <button
                key={session.name}
                type="button"
                className={`sidebar-session${session.name === selected ? ' active' : ''}`}
                ref={session.name === selected ? selectedSessionRef : undefined}
                onClick={() => onSelectSession(session.name)}
              >
                <span className="session-name">{session.name}</span>
                <span>
                  {session.turns} turns
                  {session.has_summary ? ' / summary' : ''}
                </span>
              </button>
            ))
          )}
        </div>
      </section>

      <div className="sidebar-footer">
        <div className="workspace-summary">
          <Folder size={17} />
          <div title={status?.workspace_root || ''}>
            <strong>{workspace}</strong>
            <span>{permission}</span>
          </div>
          <span
            className={`workspace-connection ${connectionState}`}
            title={connectionState}
          />
        </div>
        <div className="sidebar-footer-row">
          <button
            className="sidebar-settings"
            type="button"
            title="Settings coming soon"
            disabled
          >
            <Settings size={17} />
            <span>Settings</span>
            <small>Soon</small>
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
  isSidebarOpen,
  onPromptChange,
  onPromptKeyDown,
  onSubmit,
  onCancel,
  onOpenSidebar,
  onOpenInspector,
  onToggleRun,
  messagesEndRef,
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
  isSidebarOpen: boolean
  onPromptChange: (value: string) => void
  onPromptKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => void
  onSubmit: (event: FormEvent) => void
  onCancel: () => void
  onOpenSidebar: () => void
  onOpenInspector: (panel: InspectorPanel) => void
  onToggleRun: (id: string) => void
  messagesEndRef: React.RefObject<HTMLDivElement | null>
}) {
  const isEmpty = timeline.length === 0
  const composer = (
    <Composer
      prompt={prompt}
      canSend={canSend}
      canCancel={canCancel}
      isRunning={isRunning}
      status={status}
      variant={isEmpty ? 'home' : 'dock'}
      onPromptChange={onPromptChange}
      onPromptKeyDown={onPromptKeyDown}
      onSubmit={onSubmit}
      onCancel={onCancel}
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
            messagesEndRef={messagesEndRef}
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
        <div>
          <p className="eyebrow">Current task</p>
          <h1 title={selected}>{selected}</h1>
        </div>
      </div>
      <div className="conversation-actions">
        <span className={`connection-badge ${connectionState}`}>
          <span />
          {pendingApproval ? 'approval' : connectionState}
        </span>
        <MiniIconButton title="Open run status" onClick={() => onOpenInspector('run')}>
          <Shield size={16} />
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
  messagesEndRef,
  onToggleRun,
}: {
  items: TimelineItem[]
  messagesEndRef: React.RefObject<HTMLDivElement | null>
  onToggleRun: (id: string) => void
}) {
  return (
    <div className="message-scroll main-scroll">
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
        <div ref={messagesEndRef} />
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
  return (
    <div className="message-bubble markdown-message">
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
  return (
    <article className={`run-step ${step.kind} ${step.status}`}>
      <div className="run-step-icon">{runStepIcon(step)}</div>
      <div className="run-step-main">
        <div className="run-step-head">
          <strong>{step.title}</strong>
          <span>{step.status}</span>
        </div>
        {step.detail ? <p>{step.detail}</p> : null}
        <RunStepDetails step={step} />
      </div>
    </article>
  )
}

function RunStepDetails({ step }: { step: RunStep }) {
  const summary = step.summary
  if (!summary) return null

  return (
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
  )
}

function Composer({
  prompt,
  canSend,
  canCancel,
  isRunning,
  status,
  variant = 'dock',
  onPromptChange,
  onPromptKeyDown,
  onSubmit,
  onCancel,
}: {
  prompt: string
  canSend: boolean
  canCancel: boolean
  isRunning: boolean
  status: StatusResponse | null
  variant?: 'home' | 'dock'
  onPromptChange: (value: string) => void
  onPromptKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => void
  onSubmit: (event: FormEvent) => void
  onCancel: () => void
}) {
  const primaryLabel = isRunning ? 'Stop turn' : 'Send'
  const primaryDisabled = isRunning ? !canCancel : !canSend
  const workspace = status ? workspaceName(status.workspace_root) : 'loading'
  const permission = status
    ? formatPermissionMode(status.permissions.mode)
    : 'unknown mode'

  return (
    <form className={`composer ${variant}`} onSubmit={onSubmit}>
      <div className="composer-shell">
        <div className="composer-context" aria-label="Workspace context">
          <span title={status?.workspace_root || ''}>
            <Folder size={14} />
            {workspace}
          </span>
          <span>
            <Terminal size={14} />
            Local
          </span>
          <span>
            <Shield size={14} />
            {permission}
          </span>
        </div>
        <div className="composer-card">
          <textarea
            value={prompt}
            rows={variant === 'home' ? 3 : 2}
            disabled={isRunning}
            placeholder="Ask Morrow to edit, inspect, or explain this workspace"
            onChange={(event) => onPromptChange(event.target.value)}
            onKeyDown={onPromptKeyDown}
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
              <button
                className="composer-chip labeled"
                type="button"
                title="Plan mode coming soon"
                disabled
              >
                <ListTree size={15} />
                <span>Plan mode</span>
              </button>
            </div>
            <div className="composer-primary">
              <button
                className="composer-chip labeled"
                type="button"
                title="Model selection coming soon"
                disabled
              >
                <Bot size={15} />
                <span>Model</span>
              </button>
              <button
                aria-label={primaryLabel}
                className={`send-button composer-primary-button${isRunning ? ' stop-button' : ''}`}
                type={isRunning ? 'button' : 'submit'}
                disabled={primaryDisabled}
                onClick={isRunning ? onCancel : undefined}
              >
                {isRunning ? <Square size={17} /> : <Send size={17} />}
              </button>
            </div>
          </div>
        </div>
      </div>
    </form>
  )
}

function InspectorDrawer({
  open,
  panel,
  tools,
  activity,
  selectedEntry,
  runningTurn,
  pendingApproval,
  onClose,
  onPanelChange,
}: {
  open: boolean
  panel: InspectorPanel
  tools: ToolRun[]
  activity: ActivityItem[]
  selectedEntry?: SessionEntryResponse
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
  onClose: () => void
  onPanelChange: (panel: InspectorPanel) => void
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
}: {
  panel: InspectorPanel
  tools: ToolRun[]
  activity: ActivityItem[]
  selectedEntry?: SessionEntryResponse
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
}) {
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
    const trace = historyRunTrace(record.turn, turnIndex)
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
  turn: Session['turns'][number]['turn'],
  turnIndex: number,
): RunTrace {
  const steps: RunStep[] = turn.steps.map((step, stepIndex) => ({
    id: `history-${turnIndex}-step-${stepIndex}`,
    kind: step.kind === 'tool_call' ? 'tool' : 'model',
    status:
      step.status === 'completed'
        ? 'ok'
        : step.status === 'failed'
          ? 'error'
          : 'running',
    title:
      step.kind === 'tool_call'
        ? step.tool_name || 'Tool call'
        : 'Model call',
    detail: step.error || step.tool_call_id || undefined,
  }))

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

function runStepIcon(step: RunStep): ReactNode {
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
    case 'tools':
      return 'Tools'
    case 'recent':
      return 'Recent'
  }
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

function workspaceName(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean)
  return parts.at(-1) || path
}

function formatPermissionMode(mode: string): string {
  return mode.replaceAll('_', ' ')
}
