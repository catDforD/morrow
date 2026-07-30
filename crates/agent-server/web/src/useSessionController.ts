import { useCallback, useEffect, useRef, useState } from 'react'
import {
  SessionClient,
  SessionProtocolError,
  sessionClient as defaultSessionClient,
} from './api'
import {
  emptySessionTimelineState,
  reduceSessionFrame,
} from './sessionTimelineReducer'
import type { SessionTimelineState } from './sessionTimelineReducer'
import type {
  ClientMessage,
  ModelSelection,
  SessionDirectoryDiagnostic,
  SessionEntryResponse,
  SessionStreamFrame,
} from './types'

export type WorkspaceSessionStatus = 'loading' | 'ready' | 'degraded' | 'error'
export type SessionSelectionStatus =
  | 'none'
  | 'subscribing'
  | 'ready'
  | 'reconnecting'
  | 'error'

export interface SessionControllerOptions {
  initialSession: string | null
  client?: SessionClient
  onError?(error: unknown): void
  onNotice?(message: string): void
  onCommandData?(requestId: string, data: unknown): void
  onSelectionChange?(name: string | null): void
}

export interface SessionController {
  workspaceStatus: WorkspaceSessionStatus
  selectionStatus: SessionSelectionStatus
  sessions: SessionEntryResponse[]
  diagnostics: SessionDirectoryDiagnostic[]
  selected: string | null
  timelineState: SessionTimelineState
  pendingTurnRequest: string | null
  modelSelection: ModelSelection | null
  directoryError: string | null
  sessionError: string | null
  refreshSessions(): Promise<SessionEntryResponse[]>
  selectSession(name: string): Promise<void>
  createSession(name: string): Promise<SessionEntryResponse>
  resetSession(name: string): Promise<SessionEntryResponse>
  archiveSession(name: string): Promise<SessionEntryResponse>
  restoreSession(name: string): Promise<SessionEntryResponse>
  changeModelSelection(selection: ModelSelection): Promise<void>
  send(message: ClientMessage): void
  clearSelection(): void
}

const RECONNECT_DELAYS_MS = [250, 500, 1_000, 2_000, 5_000]
const SESSION_NAME_PATTERN = /^[A-Za-z0-9_-]+$/

export function useSessionController(
  options: SessionControllerOptions,
): SessionController {
  const clientRef = useRef(options.client ?? defaultSessionClient)
  const callbacksRef = useRef(options)
  callbacksRef.current = options

  const [workspaceStatus, setWorkspaceStatus] =
    useState<WorkspaceSessionStatus>('loading')
  const [selectionStatus, setSelectionStatus] =
    useState<SessionSelectionStatus>('none')
  const [sessions, setSessions] = useState<SessionEntryResponse[]>([])
  const [diagnostics, setDiagnostics] = useState<SessionDirectoryDiagnostic[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [timelineState, setTimelineState] = useState<SessionTimelineState>(
    emptySessionTimelineState,
  )
  const [pendingTurnRequest, setPendingTurnRequest] = useState<string | null>(null)
  const [modelSelection, setModelSelection] = useState<ModelSelection | null>(null)
  const [directoryError, setDirectoryError] = useState<string | null>(null)
  const [sessionError, setSessionError] = useState<string | null>(null)

  const sessionsRef = useRef(sessions)
  const selectedRef = useRef<string | null>(null)
  const timelineRef = useRef(timelineState)
  const pendingTurnRequestRef = useRef<string | null>(null)
  const modelSelectionRef = useRef<ModelSelection | null>(null)
  const connectionRef = useRef<Awaited<ReturnType<SessionClient['connectSession']>> | null>(null)
  const reconnectTimerRef = useRef<number | null>(null)
  const selectionGenerationRef = useRef(0)
  const connectionGenerationRef = useRef(0)
  const reconnectAttemptRef = useRef(0)
  const mountedRef = useRef(true)
  const connectRef = useRef<(
    name: string,
    selectionGeneration: number,
    reconnecting: boolean,
  ) => void>(() => {})

  const reportError = useCallback((error: unknown) => {
    callbacksRef.current.onError?.(error)
  }, [])

  const replaceTimeline = useCallback((state: SessionTimelineState) => {
    timelineRef.current = state
    setTimelineState(state)
  }, [])

  const replaceSessions = useCallback((entries: SessionEntryResponse[]) => {
    sessionsRef.current = entries
    setSessions(entries)
  }, [])

  const upsertSession = useCallback((entry: SessionEntryResponse) => {
    const entries = sessionsRef.current.filter((current) => current.name !== entry.name)
    entries.push(entry)
    entries.sort((left, right) => {
      if (left.archived !== right.archived) return left.archived ? 1 : -1
      return left.name.localeCompare(right.name)
    })
    replaceSessions(entries)
    return entries
  }, [replaceSessions])

  const clearReconnectTimer = useCallback(() => {
    if (reconnectTimerRef.current === null) return
    window.clearTimeout(reconnectTimerRef.current)
    reconnectTimerRef.current = null
  }, [])

  const closeConnection = useCallback(() => {
    connectionGenerationRef.current += 1
    clearReconnectTimer()
    const connection = connectionRef.current
    connectionRef.current = null
    connection?.close()
  }, [clearReconnectTimer])

  const clearSelection = useCallback(() => {
    selectionGenerationRef.current += 1
    closeConnection()
    reconnectAttemptRef.current = 0
    selectedRef.current = null
    setSelected(null)
    setSelectionStatus('none')
    setSessionError(null)
    pendingTurnRequestRef.current = null
    setPendingTurnRequest(null)
    modelSelectionRef.current = null
    setModelSelection(null)
    replaceTimeline(emptySessionTimelineState())
    callbacksRef.current.onSelectionChange?.(null)
  }, [closeConnection, replaceTimeline])

  const refreshSessions = useCallback(async () => {
    try {
      const directory = await clientRef.current.listSessions()
      replaceSessions(directory.sessions)
      setDiagnostics(directory.diagnostics)
      setDirectoryError(null)
      setWorkspaceStatus(directory.diagnostics.length > 0 ? 'degraded' : 'ready')
      return directory.sessions
    } catch (error) {
      const message = errorMessage(error)
      setDirectoryError(message)
      setWorkspaceStatus('error')
      reportError(error)
      throw error
    }
  }, [replaceSessions, reportError])

  const scheduleReconnect = useCallback((
    name: string,
    selectionGeneration: number,
    connectionGeneration: number,
    immediate = false,
  ) => {
    if (
      !mountedRef.current ||
      selectionGenerationRef.current !== selectionGeneration ||
      connectionGenerationRef.current !== connectionGeneration ||
      selectedRef.current !== name
    ) return

    connectionGenerationRef.current += 1
    const staleConnection = connectionRef.current
    connectionRef.current = null
    staleConnection?.close()
    clearReconnectTimer()
    setSelectionStatus('reconnecting')
    const attempt = reconnectAttemptRef.current
    const delay = immediate
      ? 0
      : RECONNECT_DELAYS_MS[Math.min(attempt, RECONNECT_DELAYS_MS.length - 1)]
    reconnectAttemptRef.current = attempt + 1
    reconnectTimerRef.current = window.setTimeout(() => {
      reconnectTimerRef.current = null
      connectRef.current(name, selectionGeneration, true)
    }, delay)
  }, [clearReconnectTimer])

  const handleFrame = useCallback((
    name: string,
    selectionGeneration: number,
    connectionGeneration: number,
    frame: SessionStreamFrame,
  ) => {
    if (
      selectionGenerationRef.current !== selectionGeneration ||
      connectionGenerationRef.current !== connectionGeneration ||
      selectedRef.current !== name
    ) return

    if (frame.type === 'command_result') {
      if (!frame.data.accepted) {
        if (pendingTurnRequestRef.current === frame.data.request_id) {
          pendingTurnRequestRef.current = null
          setPendingTurnRequest(null)
        }
        const error = new Error(frame.data.error ?? 'Session command was rejected')
        setSessionError(error.message)
        reportError(error)
      }
      return
    }
    if (frame.type === 'command_data') {
      callbacksRef.current.onCommandData?.(frame.data.request_id, frame.data.data)
      return
    }

    const previous = timelineRef.current
    const next = reduceSessionFrame(previous, frame)
    replaceTimeline(next)

    if (frame.type === 'snapshot') {
      reconnectAttemptRef.current = 0
      pendingTurnRequestRef.current = null
      setPendingTurnRequest(null)
      setSessionError(null)
      setSelectionStatus('ready')
    } else if (frame.type === 'event') {
      if (frame.data.update.type === 'turn_upserted') {
        pendingTurnRequestRef.current = null
        setPendingTurnRequest(null)
        if (frame.data.update.data.status !== 'running') {
          void refreshSessions().catch(() => undefined)
        }
      } else if (frame.data.update.type === 'notice') {
        callbacksRef.current.onNotice?.(frame.data.update.data.message)
      }
    }

    if (next.resyncRequired && !previous.resyncRequired) {
      scheduleReconnect(name, selectionGeneration, connectionGeneration, true)
    }
  }, [refreshSessions, replaceTimeline, reportError, scheduleReconnect])

  const connectSession = useCallback((
    name: string,
    selectionGeneration: number,
    reconnecting: boolean,
  ) => {
    if (
      !mountedRef.current ||
      selectionGenerationRef.current !== selectionGeneration ||
      selectedRef.current !== name
    ) return
    clearReconnectTimer()
    const connectionGeneration = connectionGenerationRef.current + 1
    connectionGenerationRef.current = connectionGeneration
    setSelectionStatus(reconnecting ? 'reconnecting' : 'subscribing')

    void clientRef.current.connectSession(name, {
      onOpen: () => undefined,
      onClose: () => {
        scheduleReconnect(name, selectionGeneration, connectionGeneration)
      },
      onMessage: (frame) => {
        handleFrame(name, selectionGeneration, connectionGeneration, frame)
      },
      onError: (error) => {
        if (
          selectionGenerationRef.current !== selectionGeneration ||
          connectionGenerationRef.current !== connectionGeneration
        ) return
        if (error instanceof SessionProtocolError) {
          connectionGenerationRef.current += 1
          setSelectionStatus('error')
          setSessionError(error.message)
          reportError(error)
          connectionRef.current?.close()
          connectionRef.current = null
          return
        }
        scheduleReconnect(name, selectionGeneration, connectionGeneration)
      },
    }).then((connection) => {
      if (
        selectionGenerationRef.current !== selectionGeneration ||
        connectionGenerationRef.current !== connectionGeneration ||
        selectedRef.current !== name
      ) {
        connection.close()
        return
      }
      connectionRef.current = connection
    }).catch((error) => {
      if (
        selectionGenerationRef.current !== selectionGeneration ||
        connectionGenerationRef.current !== connectionGeneration
      ) return
      if (error instanceof SessionProtocolError) {
        setSelectionStatus('error')
        setSessionError(error.message)
        reportError(error)
      } else {
        scheduleReconnect(name, selectionGeneration, connectionGeneration)
      }
    })
  }, [clearReconnectTimer, handleFrame, reportError, scheduleReconnect])
  connectRef.current = connectSession

  const selectSession = useCallback(async (name: string) => {
    const entry = sessionsRef.current.find(
      (session) => session.name === name && !session.archived,
    )
    if (!entry) {
      const error = new Error(`session ${JSON.stringify(name)} is not active`)
      setSessionError(error.message)
      reportError(error)
      throw error
    }

    const selectionGeneration = selectionGenerationRef.current + 1
    selectionGenerationRef.current = selectionGeneration
    closeConnection()
    reconnectAttemptRef.current = 0
    selectedRef.current = name
    setSelected(name)
    setSessionError(null)
    pendingTurnRequestRef.current = null
    setPendingTurnRequest(null)
    modelSelectionRef.current = null
    setModelSelection(null)
    replaceTimeline(emptySessionTimelineState())
    callbacksRef.current.onSelectionChange?.(name)
    connectSession(name, selectionGeneration, false)

    try {
      const selection = await clientRef.current.getModelSelection(name)
      if (
        selectionGenerationRef.current !== selectionGeneration ||
        selectedRef.current !== name
      ) return
      modelSelectionRef.current = selection
      setModelSelection(selection)
    } catch (error) {
      if (selectionGenerationRef.current !== selectionGeneration) return
      setSessionError(errorMessage(error))
      reportError(error)
    }
  }, [closeConnection, connectSession, replaceTimeline, reportError])

  const createSession = useCallback(async (rawName: string) => {
    const name = rawName.trim()
    if (!SESSION_NAME_PATTERN.test(name)) {
      throw new Error("session name must use ASCII letters, digits, '-' or '_'")
    }
    const entry = await clientRef.current.createSession(name)
    upsertSession(entry)
    setDirectoryError(null)
    setWorkspaceStatus(diagnostics.length > 0 ? 'degraded' : 'ready')
    await selectSession(entry.name)
    return entry
  }, [diagnostics.length, selectSession, upsertSession])

  const resetSession = useCallback(async (name: string) => {
    const wasSelected = selectedRef.current === name
    if (wasSelected) {
      selectionGenerationRef.current += 1
      closeConnection()
      setSelectionStatus('subscribing')
    }
    try {
      const entry = await clientRef.current.resetSession(name)
      upsertSession(entry)
      if (wasSelected) await selectSession(name)
      return entry
    } catch (error) {
      if (wasSelected && sessionsRef.current.some((entry) => entry.name === name)) {
        await selectSession(name).catch(() => undefined)
      }
      throw error
    }
  }, [closeConnection, selectSession, upsertSession])

  const archiveSession = useCallback(async (name: string) => {
    const wasSelected = selectedRef.current === name
    if (wasSelected) {
      selectionGenerationRef.current += 1
      closeConnection()
      setSelectionStatus('subscribing')
    }
    try {
      const entry = await clientRef.current.archiveSession(name)
      const entries = upsertSession(entry)
      if (wasSelected) {
        const next = entries.find((candidate) => !candidate.archived)
        if (next) await selectSession(next.name)
        else clearSelection()
      }
      return entry
    } catch (error) {
      if (wasSelected && sessionsRef.current.some((entry) => entry.name === name)) {
        await selectSession(name).catch(() => undefined)
      }
      throw error
    }
  }, [clearSelection, closeConnection, selectSession, upsertSession])

  const restoreSession = useCallback(async (name: string) => {
    const entry = await clientRef.current.restoreSession(name)
    upsertSession(entry)
    await selectSession(entry.name)
    return entry
  }, [selectSession, upsertSession])

  const changeModelSelection = useCallback(async (selection: ModelSelection) => {
    const name = selectedRef.current
    if (!name) throw new Error('no session is selected')
    const selectionGeneration = selectionGenerationRef.current
    const previous = modelSelectionRef.current
    modelSelectionRef.current = selection
    setModelSelection(selection)
    try {
      const saved = await clientRef.current.setModelSelection(name, selection)
      if (
        selectedRef.current !== name ||
        selectionGenerationRef.current !== selectionGeneration
      ) return
      modelSelectionRef.current = saved
      setModelSelection(saved)
    } catch (error) {
      if (
        selectedRef.current === name &&
        selectionGenerationRef.current === selectionGeneration
      ) {
        modelSelectionRef.current = previous
        setModelSelection(previous)
        setSessionError(errorMessage(error))
      }
      reportError(error)
      throw error
    }
  }, [reportError])

  const send = useCallback((message: ClientMessage) => {
    const connection = connectionRef.current
    if (selectionStatus !== 'ready' || !connection?.isOpen) {
      throw new Error('session is not ready')
    }
    if (message.type === 'start_turn') {
      pendingTurnRequestRef.current = message.data.request_id
      setPendingTurnRequest(message.data.request_id)
    }
    try {
      connection.send(message)
    } catch (error) {
      if (
        message.type === 'start_turn' &&
        pendingTurnRequestRef.current === message.data.request_id
      ) {
        pendingTurnRequestRef.current = null
        setPendingTurnRequest(null)
      }
      throw error
    }
  }, [selectionStatus])

  useEffect(() => {
    mountedRef.current = true
    let cancelled = false
    void refreshSessions()
      .then(async (entries) => {
        if (cancelled) return
        const active = entries.filter((entry) => !entry.archived)
        if (options.initialSession) {
          const requested = active.find(
            (entry) => entry.name === options.initialSession,
          )
          if (requested) {
            await selectSession(requested.name)
            return
          }
          clearSelection()
          callbacksRef.current.onNotice?.(
            `Session ${JSON.stringify(options.initialSession)} is unavailable.`,
          )
          return
        }
        const next = active[0]
        if (next) await selectSession(next.name)
        else clearSelection()
      })
      .catch(() => {
        if (!cancelled) clearSelection()
      })
    return () => {
      cancelled = true
      mountedRef.current = false
      selectionGenerationRef.current += 1
      closeConnection()
    }
  }, [])

  return {
    workspaceStatus,
    selectionStatus,
    sessions,
    diagnostics,
    selected,
    timelineState,
    pendingTurnRequest,
    modelSelection,
    directoryError,
    sessionError,
    refreshSessions,
    selectSession,
    createSession,
    resetSession,
    archiveSession,
    restoreSession,
    changeModelSelection,
    send,
    clearSelection,
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
