import type { FormEvent, KeyboardEvent, ReactNode } from 'react'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  Activity,
  Bot,
  CheckCircle2,
  CircleAlert,
  Clock3,
  Files,
  ListTree,
  Moon,
  RefreshCw,
  RotateCcw,
  Send,
  Shield,
  Square,
  Sun,
  Terminal,
  Wrench,
  X,
} from 'lucide-react'
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
  ToolExecutionSummary,
  ToolRun,
  UiMessage,
} from './types'

type Tab = 'chat' | 'activity' | 'tools' | 'sessions'
type ConnectionStatus = 'connecting' | 'connected' | 'disconnected'

const tabs: { id: Tab; label: string; icon: ReactNode }[] = [
  { id: 'chat', label: 'Chat', icon: <Bot size={18} /> },
  { id: 'activity', label: 'Activity', icon: <Activity size={18} /> },
  { id: 'tools', label: 'Tools', icon: <Wrench size={18} /> },
  { id: 'sessions', label: 'Sessions', icon: <ListTree size={18} /> },
]

const emptySessionEntry = (name: string): SessionEntryResponse => ({
  name,
  path: '',
  turns: 0,
  active_messages: 0,
  summarized_turns: 0,
  has_summary: false,
})

export default function App() {
  const [activeTab, setActiveTab] = useState<Tab>('chat')
  const [status, setStatus] = useState<StatusResponse | null>(null)
  const [sessions, setSessions] = useState<SessionEntryResponse[]>([])
  const [selected, setSelected] = useState('default')
  const [messages, setMessages] = useState<UiMessage[]>([])
  const [tools, setTools] = useState<ToolRun[]>([])
  const [activity, setActivity] = useState<ActivityItem[]>([])
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
  const idRef = useRef(0)
  const selectionRef = useRef(0)
  const messagesEndRef = useRef<HTMLDivElement | null>(null)

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

  const addMessage = useCallback(
    (role: UiMessage['role'], content: string) => {
      const id = nextId(role)
      setMessages((items) => [...items, { id, role, content }])
      return id
    },
    [nextId],
  )

  const showError = useCallback(
    (error: unknown) => {
      const message = error instanceof Error ? error.message : String(error)
      addMessage('tool', message)
      recordActivity('Error', message, 'error')
    },
    [addMessage, recordActivity],
  )

  const appendAssistantDelta = useCallback(
    (text: string) => {
      if (!assistantMessageIdRef.current) {
        const id = nextId('assistant')
        assistantMessageIdRef.current = id
        setMessages((items) => [...items, { id, role: 'assistant', content: text }])
        return
      }

      const id = assistantMessageIdRef.current
      setMessages((items) =>
        items.map((item) =>
          item.id === id ? { ...item, content: item.content + text } : item,
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
          recordActivity('Turn started', selectedRef.current, 'running')
          break
        case 'text_delta':
          appendAssistantDelta(event.data)
          break
        case 'agent_message':
          if (!assistantMessageIdRef.current && event.data.trim()) {
            addMessage('assistant', event.data)
          }
          assistantMessageIdRef.current = null
          break
        case 'tool_call_started':
          upsertTool(event.data.id, event.data.name, 'running')
          recordActivity('Tool started', event.data.name, 'running')
          break
        case 'tool_call_finished':
          upsertTool(
            event.data.id,
            event.data.name,
            event.data.ok ? 'ok' : 'error',
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
          recordActivity('Approval requested', event.data.reason, 'approval')
          break
        case 'approval_resolved':
          setPendingApproval(null)
          recordActivity(
            event.data.approved ? 'Approval granted' : 'Approval denied',
            event.data.request_id,
            event.data.approved ? 'ok' : 'error',
          )
          break
        case 'turn_completed':
          setRunningTurn(null)
          assistantMessageIdRef.current = null
          recordActivity('Turn completed', selectedRef.current, 'ok')
          break
        case 'error':
          setRunningTurn(null)
          showError(event.data)
          break
      }
    },
    [addMessage, appendAssistantDelta, recordActivity, showError, upsertTool],
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
      setRunningTurn(null)
      setPendingApproval(null)
      setTools([])
      setActivity([])
      assistantMessageIdRef.current = null
      setMessages([])
      closeSocket()
      history.replaceState(null, '', `?session=${encodeURIComponent(name)}`)

      try {
        const document = await fetchJson<SessionDocument>(
          `/api/sessions/${encodeURIComponent(name)}`,
        )
        if (selectionRef.current !== selectionId) return
        setMessages(sessionMessages(document.session))
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
    document.documentElement.classList.toggle('dark', theme === 'dark')
    localStorage.setItem('morrow-theme', theme)
  }, [theme])

  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ block: 'end' })
  }, [messages])

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

  const isRunning = Boolean(runningTurn)
  const canSend = connection === 'connected' && !isRunning && prompt.trim().length > 0
  const canCancel = Boolean(runningTurn?.turn_id && runningTurn.turn_id !== 'pending')

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    const trimmed = prompt.trim()
    if (!trimmed || !canSend) return
    try {
      addMessage('user', trimmed)
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

  const refresh = async () => {
    try {
      await loadSessions()
      await selectSession(selectedRef.current)
    } catch (error) {
      showError(error)
    }
  }

  const resetSession = async () => {
    try {
      await fetchJson<SessionDocument>(
        `/api/sessions/${encodeURIComponent(selectedRef.current)}/reset`,
        { method: 'POST' },
      )
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
      <div className="app-frame">
        <BrandRail />
        <main className="window-main">
          <TopTabs
            activeTab={activeTab}
            connection={connection}
            theme={theme}
            onRefresh={() => void refresh()}
            onThemeToggle={() =>
              setTheme((current) => (current === 'dark' ? 'light' : 'dark'))
            }
            onTabChange={setActiveTab}
          />
          {activeTab === 'chat' ? (
            <ChatView
              sessions={sessions}
              status={status}
              connection={connection}
              selected={selected}
              selectedEntry={selectedEntry}
              messages={messages}
              tools={tools}
              activity={activity}
              runningTurn={runningTurn}
              pendingApproval={pendingApproval}
              prompt={prompt}
              canSend={canSend}
              canCancel={canCancel}
              isRunning={isRunning}
              onPromptChange={setPrompt}
              onPromptKeyDown={handlePromptKeyDown}
              onSubmit={handleSubmit}
              onCancel={cancelTurn}
              onReset={() => void resetSession()}
              onSelectSession={(name) => void selectSession(name)}
              messagesEndRef={messagesEndRef}
            />
          ) : activeTab === 'activity' ? (
            <ActivityView activity={activity} />
          ) : activeTab === 'tools' ? (
            <ToolsView tools={tools} />
          ) : (
            <SessionsView
              sessions={sessions}
              selected={selected}
              onSelect={(name) => {
                setActiveTab('chat')
                void selectSession(name)
              }}
              onRefresh={() => void refresh()}
            />
          )}
        </main>
      </div>
      <ApprovalDialog
        request={pendingApproval}
        onApprove={() => sendApproval(true)}
        onDeny={() => sendApproval(false)}
      />
    </>
  )
}

function BrandRail() {
  return (
    <aside className="brand-rail">
      <div className="brand-word">MORROW</div>
    </aside>
  )
}

function TopTabs({
  activeTab,
  connection,
  theme,
  onRefresh,
  onThemeToggle,
  onTabChange,
}: {
  activeTab: Tab
  connection: ConnectionStatus
  theme: 'light' | 'dark'
  onRefresh: () => void
  onThemeToggle: () => void
  onTabChange: (tab: Tab) => void
}) {
  return (
    <nav className="top-tabs">
      <div className="tab-list">
        {tabs.map((tab) => (
          <button
            key={tab.id}
            type="button"
            className={`tab-button${activeTab === tab.id ? ' active' : ''}`}
            onClick={() => onTabChange(tab.id)}
          >
            {tab.icon}
            <span>{tab.label}</span>
          </button>
        ))}
      </div>
      <div className="toolbar">
        <span className={`connection-dot ${connection}`} title={connection} />
        <IconButton title="Refresh" onClick={onRefresh}>
          <RefreshCw size={18} />
        </IconButton>
        <IconButton title="Toggle theme" onClick={onThemeToggle}>
          {theme === 'dark' ? <Sun size={18} /> : <Moon size={18} />}
        </IconButton>
      </div>
    </nav>
  )
}

function ChatView({
  sessions,
  status,
  connection,
  selected,
  selectedEntry,
  messages,
  tools,
  activity,
  runningTurn,
  pendingApproval,
  prompt,
  canSend,
  canCancel,
  isRunning,
  onPromptChange,
  onPromptKeyDown,
  onSubmit,
  onCancel,
  onReset,
  onSelectSession,
  messagesEndRef,
}: {
  sessions: SessionEntryResponse[]
  status: StatusResponse | null
  connection: ConnectionStatus
  selected: string
  selectedEntry?: SessionEntryResponse
  messages: UiMessage[]
  tools: ToolRun[]
  activity: ActivityItem[]
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
  prompt: string
  canSend: boolean
  canCancel: boolean
  isRunning: boolean
  onPromptChange: (value: string) => void
  onPromptKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => void
  onSubmit: (event: FormEvent) => void
  onCancel: () => void
  onReset: () => void
  onSelectSession: (name: string) => void
  messagesEndRef: React.RefObject<HTMLDivElement | null>
}) {
  return (
    <section className="chat-grid">
      <SessionRail
        sessions={sessions}
        status={status}
        connection={connection}
        runningTurn={runningTurn}
        selected={selected}
        onSelect={onSelectSession}
      />
      <section className="conversation-panel">
        <ConversationHeader selected={selected} selectedEntry={selectedEntry} />
        <div className="message-scroll main-scroll">
          {messages.length === 0 ? (
            <EmptyState />
          ) : (
            messages.map((message) => (
              <MessageBubble key={message.id} message={message} />
            ))
          )}
          <div ref={messagesEndRef} />
        </div>
        <Composer
          prompt={prompt}
          canSend={canSend}
          canCancel={canCancel}
          isRunning={isRunning}
          onPromptChange={onPromptChange}
          onPromptKeyDown={onPromptKeyDown}
          onSubmit={onSubmit}
          onCancel={onCancel}
          onReset={onReset}
        />
      </section>
      <RunInspector
        tools={tools}
        activity={activity}
        runningTurn={runningTurn}
        pendingApproval={pendingApproval}
      />
    </section>
  )
}

function SessionRail({
  sessions,
  status,
  connection,
  runningTurn,
  selected,
  onSelect,
}: {
  sessions: SessionEntryResponse[]
  status: StatusResponse | null
  connection: ConnectionStatus
  runningTurn: RunningTurnSnapshot | null
  selected: string
  onSelect: (name: string) => void
}) {
  return (
    <aside className="session-rail">
      <div className="panel-title">
        <Files size={18} />
        <span>Sessions</span>
      </div>
      <div className="session-list main-scroll">
        {sessions.map((session) => (
          <button
            key={session.name}
            type="button"
            className={`session-card${session.name === selected ? ' active' : ''}`}
            onClick={() => onSelect(session.name)}
          >
            <span className="session-name">{session.name}</span>
            <span className="session-stats">
              {session.turns} turns
              {session.has_summary ? ' / summary' : ''}
            </span>
          </button>
        ))}
      </div>
      <WorkspaceCard
        status={status}
        connection={connection}
        running={Boolean(runningTurn)}
      />
    </aside>
  )
}

function WorkspaceCard({
  status,
  connection,
  running,
}: {
  status: StatusResponse | null
  connection: ConnectionStatus
  running: boolean
}) {
  return (
    <section className="workspace-card">
      <div>
        <p className="rail-label">Workspace</p>
        <p className="rail-value" title={status?.workspace_root || ''}>
          {status ? workspaceName(status.workspace_root) : 'loading'}
        </p>
      </div>
      <div>
        <p className="rail-label">Mode</p>
        <p className="rail-value">
          {status ? formatPermissionMode(status.permissions.mode) : 'unknown'}
        </p>
      </div>
      <StatusPill status={connection} running={running} />
    </section>
  )
}

function ConversationHeader({
  selected,
  selectedEntry,
}: {
  selected: string
  selectedEntry?: SessionEntryResponse
}) {
  return (
    <header className="conversation-header">
      <div>
        <p className="eyebrow">Current session</p>
        <h1>{selected}</h1>
      </div>
      <div className="metric-strip">
        <Metric label="turns" value={String(selectedEntry?.turns ?? 0)} />
        <Metric
          label="active"
          value={String(selectedEntry?.active_messages ?? 0)}
        />
        <Metric
          label="summary"
          value={selectedEntry?.has_summary ? 'yes' : 'no'}
        />
      </div>
    </header>
  )
}

function MessageBubble({ message }: { message: UiMessage }) {
  return (
    <article className={`message-row ${message.role}`}>
      <div className="message-role">{message.role}</div>
      <pre className="message-bubble">{message.content}</pre>
    </article>
  )
}

function EmptyState() {
  return (
    <div className="empty-state">
      <Bot size={32} />
      <p>Morrow is ready.</p>
    </div>
  )
}

function Composer({
  prompt,
  canSend,
  canCancel,
  isRunning,
  onPromptChange,
  onPromptKeyDown,
  onSubmit,
  onCancel,
  onReset,
}: {
  prompt: string
  canSend: boolean
  canCancel: boolean
  isRunning: boolean
  onPromptChange: (value: string) => void
  onPromptKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => void
  onSubmit: (event: FormEvent) => void
  onCancel: () => void
  onReset: () => void
}) {
  return (
    <form className="composer" onSubmit={onSubmit}>
      <textarea
        value={prompt}
        rows={3}
        disabled={isRunning}
        onChange={(event) => onPromptChange(event.target.value)}
        onKeyDown={onPromptKeyDown}
      />
      <div className="composer-actions">
        <IconButton title="Reset session" disabled={isRunning} onClick={onReset}>
          <RotateCcw size={18} />
        </IconButton>
        <IconButton title="Cancel turn" disabled={!canCancel} onClick={onCancel}>
          <Square size={18} />
        </IconButton>
        <button className="send-button" type="submit" disabled={!canSend}>
          <Send size={18} />
          <span>Send</span>
        </button>
      </div>
    </form>
  )
}

function RunInspector({
  tools,
  activity,
  runningTurn,
  pendingApproval,
}: {
  tools: ToolRun[]
  activity: ActivityItem[]
  runningTurn: RunningTurnSnapshot | null
  pendingApproval: ApprovalRequest | null
}) {
  const lastItems = activity.slice(-5).reverse()
  return (
    <aside className="inspector main-scroll">
      <div className="panel-title">
        <Shield size={18} />
        <span>Run</span>
      </div>
      <div className="status-card">
        <p className="eyebrow">Turn</p>
        <strong>{runningTurn ? runningTurn.turn_id : 'idle'}</strong>
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
      <ActivityList items={lastItems} compact />
    </aside>
  )
}

function ActivityView({ activity }: { activity: ActivityItem[] }) {
  return (
    <section className="single-view main-scroll">
      <div className="single-header">
        <Activity size={22} />
        <h1>Activity</h1>
      </div>
      <ActivityList items={[...activity].reverse()} />
    </section>
  )
}

function ToolsView({ tools }: { tools: ToolRun[] }) {
  return (
    <section className="single-view main-scroll">
      <div className="single-header">
        <Wrench size={22} />
        <h1>Tools</h1>
      </div>
      <ToolList tools={tools} />
    </section>
  )
}

function SessionsView({
  sessions,
  selected,
  onSelect,
  onRefresh,
}: {
  sessions: SessionEntryResponse[]
  selected: string
  onSelect: (name: string) => void
  onRefresh: () => void
}) {
  return (
    <section className="single-view main-scroll">
      <div className="single-header split">
        <div>
          <Files size={22} />
          <h1>Sessions</h1>
        </div>
        <IconButton title="Refresh sessions" onClick={onRefresh}>
          <RefreshCw size={18} />
        </IconButton>
      </div>
      <div className="session-table">
        {sessions.map((session) => (
          <button
            key={session.name}
            type="button"
            className={`session-row${session.name === selected ? ' active' : ''}`}
            onClick={() => onSelect(session.name)}
          >
            <span>{session.name}</span>
            <span>{session.turns}</span>
            <span>{session.active_messages}</span>
            <span>{session.has_summary ? 'yes' : 'no'}</span>
          </button>
        ))}
      </div>
    </section>
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

function StatusPill({
  status,
  running,
}: {
  status: ConnectionStatus
  running: boolean
}) {
  const text = running ? 'running' : status
  return <span className={`status-pill ${running ? 'running' : status}`}>{text}</span>
}

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <span className="metric">
      <strong>{value}</strong>
      <span>{label}</span>
    </span>
  )
}

function sessionMessages(session: Session): UiMessage[] {
  const source =
    session.turns.length > 0
      ? session.turns.flatMap((record) => record.messages)
      : session.active_thread.messages

  return source
    .map((message, index) => messageToUi(message, index))
    .filter((message): message is UiMessage => Boolean(message))
}

function messageToUi(message: Message, index: number): UiMessage | null {
  if (message.role === 'system') return null
  const content = message.content ?? formatToolCalls(message)
  if (!content) return null
  return {
    id: `history-${index}-${message.role}`,
    role: message.role,
    content,
  }
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
