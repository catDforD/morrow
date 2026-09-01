import {
  getDesktopPlatform,
  getDesktopShellState,
  listenRemoteEvents,
  remoteRequest,
} from './desktop'
import type {
  ClientMessage,
  ModelSelection,
  SessionDirectoryResponse,
  SessionEntryResponse,
  SessionModelSelectionResponse,
  SessionSnapshot,
  SessionStreamFrame,
  SessionUpdateEnvelope,
} from './types'

export const SESSION_DIRECTORY_SCHEMA_VERSION = 1
export const SESSION_STREAM_SCHEMA_VERSION = 3

export class SessionProtocolError extends Error {
  readonly fatal = true
}

export interface SessionConnection {
  isOpen: boolean
  send(message: ClientMessage): void
  close(): void
}

export interface SessionConnectionHandlers {
  onOpen(): void
  onClose(): void
  onMessage(message: SessionStreamFrame): void
  onError(error: unknown): void
}

export interface AppTransport {
  fetchJson<T>(url: string, options?: RequestInit): Promise<T>
  openSessionConnection(
    name: string,
    handlers: SessionConnectionHandlers,
  ): Promise<SessionConnection>
}

export class BrowserTransport implements AppTransport {
  async fetchJson<T>(url: string, options?: RequestInit): Promise<T> {
    const response = await fetch(url, options)
    if (!response.ok) {
      const body = await response.json().catch(() => ({}))
      const message =
        typeof body.error === 'string'
          ? body.error
          : `${response.status} ${response.statusText}`
      throw new Error(message)
    }
    if (response.status === 204) return undefined as T
    return response.json() as Promise<T>
  }

  async openSessionConnection(
    name: string,
    handlers: SessionConnectionHandlers,
  ): Promise<SessionConnection> {
    const socket = new WebSocket(sessionSocketUrl(name))
    const connection: SessionConnection = {
      get isOpen() {
        return socket.readyState === WebSocket.OPEN
      },
      send(message) {
        socket.send(JSON.stringify(message))
      },
      close() {
        socket.close()
      },
    }
    socket.addEventListener('open', handlers.onOpen)
    socket.addEventListener('close', handlers.onClose)
    socket.addEventListener('error', handlers.onError)
    socket.addEventListener('message', (event) => {
      try {
        handlers.onMessage(parseSessionStreamFrame(JSON.parse(event.data), name))
      } catch (error) {
        handlers.onError(error)
        socket.close()
      }
    })
    return connection
  }
}

export class DesktopTransport implements AppTransport {
  async fetchJson<T>(url: string, options?: RequestInit): Promise<T> {
    const rawBody = options?.body
    const body =
      typeof rawBody === 'string' && rawBody.length > 0
        ? JSON.parse(rawBody)
        : undefined
    const response = await remoteRequest<{
      type: 'http'
      data: { status: number; body?: unknown }
    }>({
      type: 'http',
      data: {
        method: options?.method ?? 'GET',
        path: url,
        body,
      },
    })
    if (response.data.status < 200 || response.data.status >= 300) {
      const errorBody = response.data.body as { error?: string } | undefined
      throw new Error(errorBody?.error ?? `Remote request failed: ${response.data.status}`)
    }
    return response.data.body as T
  }

  async openSessionConnection(
    name: string,
    handlers: SessionConnectionHandlers,
  ): Promise<SessionConnection> {
    let open = false
    let closedByUser = false
    let closeNotified = false
    let subscriptionId = ''
    let snapshotApplied = false
    let earlyFrames: unknown[] = []
    const notifyClose = () => {
      if (closeNotified) return
      closeNotified = true
      handlers.onClose()
    }
    const unsubscribe = (id: string) => {
      if (!id) return
      void remoteRequest({
        type: 'unsubscribe_session',
        data: { subscription_id: id },
      }).catch(() => undefined)
    }
    const subscribe = async () => {
      const nextSubscriptionId = createSubscriptionId()
      subscriptionId = nextSubscriptionId
      snapshotApplied = false
      earlyFrames = []
      const response = await remoteRequest<{
        type: 'session_subscribed'
        data: { subscription_id: string; snapshot: unknown }
      }>({
        type: 'subscribe_session',
        data: { session: name, subscription_id: nextSubscriptionId },
      })
      if (closedByUser) {
        unsubscribe(nextSubscriptionId)
        return
      }
      if (response.data.subscription_id !== nextSubscriptionId) {
        throw new Error('Remote returned a mismatched session subscription')
      }
      handlers.onMessage(parseSessionStreamFrame(response.data.snapshot, name))
      snapshotApplied = true
      for (const frame of earlyFrames) {
        handlers.onMessage(parseSessionStreamFrame(frame, name))
      }
      earlyFrames = []
      open = true
      handlers.onOpen()
    }
    const unlisten = await listenRemoteEvents((envelope) => {
      const event = envelope.message.data
      if (
        event.type === 'session_message' &&
        event.data.subscription_id === subscriptionId
      ) {
        if (snapshotApplied) {
          try {
            handlers.onMessage(parseSessionStreamFrame(event.data.message, name))
          } catch (error) {
            handlers.onError(error)
          }
        } else earlyFrames.push(event.data.message)
      } else if (event.type === 'worker_exited') {
        open = false
        snapshotApplied = false
        notifyClose()
      }
    })
    try {
      await subscribe()
    } catch (error) {
      unlisten()
      unsubscribe(subscriptionId)
      throw error
    }

    return {
      get isOpen() {
        return open
      },
      send(message) {
        if (!open) throw new Error('remote session is not connected')
        void remoteRequest<
          | { type: 'ack' }
          | { type: 'session_command'; data: { message: unknown } }
        >({
          type: 'session_message',
          data: { session: name, message },
        })
          .then((response) => {
            if (response.type === 'session_command') {
              handlers.onMessage(
                parseSessionStreamFrame(response.data.message, name),
              )
            }
          })
          .catch(handlers.onError)
      },
      close() {
        if (closedByUser) return
        closedByUser = true
        open = false
        snapshotApplied = false
        unlisten()
        unsubscribe(subscriptionId)
        notifyClose()
      },
    }
  }
}

function createSubscriptionId(): string {
  return typeof globalThis.crypto?.randomUUID === 'function'
    ? `subscription-${globalThis.crypto.randomUUID()}`
    : `subscription-${Date.now()}-${Math.random().toString(16).slice(2)}`
}

const browserTransport = new BrowserTransport()
const desktopTransport = new DesktopTransport()

async function currentTransport(): Promise<AppTransport> {
  if (!getDesktopPlatform()) return browserTransport
  try {
    return (await getDesktopShellState()).activeWorkspace?.kind === 'wsl'
      ? desktopTransport
      : browserTransport
  } catch {
    return browserTransport
  }
}

export async function fetchJson<T>(url: string, options?: RequestInit): Promise<T> {
  return (await currentTransport()).fetchJson<T>(url, options)
}

export function sessionSocketUrl(name: string): string {
  const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${protocol}//${location.host}/api/sessions/${encodeURIComponent(name)}/ws`
}

export async function openSessionConnection(
  name: string,
  handlers: SessionConnectionHandlers,
): Promise<SessionConnection> {
  return (await currentTransport()).openSessionConnection(name, handlers)
}

export class SessionClient {
  constructor(private readonly transport?: AppTransport) {}

  private async fetch<T>(url: string, options?: RequestInit): Promise<T> {
    const transport = this.transport ?? (await currentTransport())
    return transport.fetchJson<T>(url, options)
  }

  async listSessions(): Promise<SessionDirectoryResponse> {
    const response = await this.fetch<unknown>('/api/sessions')
    return parseSessionDirectory(response)
  }

  async createSession(name: string): Promise<SessionEntryResponse> {
    const response = await this.fetch<unknown>('/api/sessions', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name }),
    })
    return parseSessionEntry(response)
  }

  async resetSession(name: string): Promise<SessionEntryResponse> {
    const response = await this.fetch<unknown>(
      `/api/sessions/${encodeURIComponent(name)}/reset`,
      { method: 'POST' },
    )
    return parseSessionEntry(response)
  }

  async archiveSession(name: string): Promise<SessionEntryResponse> {
    const response = await this.fetch<unknown>(
      `/api/sessions/${encodeURIComponent(name)}/archive`,
      { method: 'POST' },
    )
    return parseSessionEntry(response)
  }

  async restoreSession(name: string): Promise<SessionEntryResponse> {
    const response = await this.fetch<unknown>(
      `/api/sessions/${encodeURIComponent(name)}/restore`,
      { method: 'POST' },
    )
    return parseSessionEntry(response)
  }

  async getModelSelection(name: string): Promise<ModelSelection | null> {
    const response = await this.fetch<unknown>(
      `/api/sessions/${encodeURIComponent(name)}/model-selection`,
    )
    return parseSessionModelSelection(response).selection ?? null
  }

  async setModelSelection(
    name: string,
    selection: ModelSelection,
  ): Promise<ModelSelection | null> {
    const response = await this.fetch<unknown>(
      `/api/sessions/${encodeURIComponent(name)}/model-selection`,
      {
        method: 'PUT',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(selection),
      },
    )
    return parseSessionModelSelection(response).selection ?? null
  }

  connectSession(
    name: string,
    handlers: SessionConnectionHandlers,
  ): Promise<SessionConnection> {
    return this.transport
      ? this.transport.openSessionConnection(name, handlers)
      : openSessionConnection(name, handlers)
  }
}

export const sessionClient = new SessionClient()

function parseSessionDirectory(value: unknown): SessionDirectoryResponse {
  const directory = expectRecord(value, 'session directory')
  if (directory.schema_version !== SESSION_DIRECTORY_SCHEMA_VERSION) {
    throw new SessionProtocolError(
      `unsupported session directory v${String(directory.schema_version)}`,
    )
  }
  if (!Array.isArray(directory.sessions) || !Array.isArray(directory.diagnostics)) {
    throw new SessionProtocolError('invalid session directory payload')
  }
  for (const entry of directory.sessions) parseSessionEntry(entry)
  for (const value of directory.diagnostics) {
    const diagnostic = expectRecord(value, 'session directory diagnostic')
    if (
      (diagnostic.name !== undefined &&
        diagnostic.name !== null &&
        typeof diagnostic.name !== 'string') ||
      typeof diagnostic.path !== 'string' ||
      typeof diagnostic.message !== 'string'
    ) {
      throw new SessionProtocolError('invalid session directory diagnostic')
    }
  }
  return directory as unknown as SessionDirectoryResponse
}

function parseSessionEntry(value: unknown): SessionEntryResponse {
  const entry = expectRecord(value, 'session entry')
  if (
    typeof entry.name !== 'string' ||
    typeof entry.path !== 'string' ||
    typeof entry.turns !== 'number' ||
    typeof entry.active_messages !== 'number' ||
    typeof entry.summarized_turns !== 'number' ||
    typeof entry.has_summary !== 'boolean' ||
    typeof entry.archived !== 'boolean'
  ) {
    throw new SessionProtocolError('invalid session entry payload')
  }
  return entry as unknown as SessionEntryResponse
}

function parseSessionModelSelection(
  value: unknown,
): SessionModelSelectionResponse {
  const response = expectRecord(value, 'session model selection')
  if (typeof response.inherited !== 'boolean') {
    throw new SessionProtocolError('invalid session model selection')
  }
  if (response.selection !== undefined && response.selection !== null) {
    validateModelSelection(response.selection)
  }
  return response as unknown as SessionModelSelectionResponse
}

export function parseSessionStreamFrame(
  value: unknown,
  expectedSession?: string,
): SessionStreamFrame {
  const frame = expectRecord(value, 'session frame')
  if (typeof frame.type !== 'string' || !('data' in frame)) {
    throw new SessionProtocolError('invalid session frame')
  }
  switch (frame.type) {
    case 'snapshot':
      validateSnapshot(frame.data, expectedSession)
      break
    case 'event':
      validateEvent(frame.data)
      break
    case 'resync_required': {
      const data = expectRecord(frame.data, 'resync frame')
      if (typeof data.reason !== 'string') {
        throw new SessionProtocolError('invalid resync frame')
      }
      break
    }
    case 'command_result': {
      const data = expectRecord(frame.data, 'command result')
      if (typeof data.request_id !== 'string' || typeof data.accepted !== 'boolean') {
        throw new SessionProtocolError('invalid command result frame')
      }
      break
    }
    case 'command_data': {
      const data = expectRecord(frame.data, 'command data')
      if (typeof data.request_id !== 'string') {
        throw new SessionProtocolError('invalid command data frame')
      }
      break
    }
    default:
      throw new SessionProtocolError(`unsupported session frame ${frame.type}`)
  }
  return frame as unknown as SessionStreamFrame
}

function validateSnapshot(value: unknown, expectedSession?: string): SessionSnapshot {
  const snapshot = expectRecord(value, 'session snapshot')
  if (snapshot.schema_version !== SESSION_STREAM_SCHEMA_VERSION) {
    throw new SessionProtocolError(
      `unsupported session stream v${String(snapshot.schema_version)}`,
    )
  }
  if (
    typeof snapshot.session_name !== 'string' ||
    typeof snapshot.session_id !== 'string' ||
    !isNonNegativeInteger(snapshot.revision) ||
    !Array.isArray(snapshot.approvals) ||
    !Array.isArray(snapshot.subagents)
  ) {
    throw new SessionProtocolError('invalid session snapshot')
  }
  if (expectedSession && snapshot.session_name !== expectedSession) {
    throw new SessionProtocolError(
      `snapshot session mismatch: expected ${expectedSession}, received ${snapshot.session_name}`,
    )
  }
  const cursor = expectRecord(snapshot.cursor, 'stream cursor')
  if (
    typeof cursor.stream_id !== 'string' ||
    !isNonNegativeInteger(cursor.sequence)
  ) {
    throw new SessionProtocolError('invalid stream cursor')
  }
  const session = expectRecord(snapshot.session, 'session projection')
  if (
    typeof session.session_id !== 'string' ||
    !isNonNegativeInteger(session.revision) ||
    !Array.isArray(session.turns) ||
    !Array.isArray(session.middleware_audit) ||
    !Array.isArray(session.diagnostics) ||
    !session.diagnostics.every((diagnostic) => typeof diagnostic === 'string')
  ) {
    throw new SessionProtocolError('invalid session projection')
  }
  if (
    session.session_id !== snapshot.session_id ||
    session.revision !== snapshot.revision
  ) {
    throw new SessionProtocolError('inconsistent session snapshot identity')
  }
  validateContextProjection(session.context)
  for (const turn of session.turns) validateTurnProjection(turn)
  for (const invocation of session.middleware_audit) {
    validateMiddlewareInvocation(invocation)
  }
  if (snapshot.active_operation !== undefined && snapshot.active_operation !== null) {
    validateOperationProjection(snapshot.active_operation)
  }
  validatePermissionProfile(snapshot.permissions)
  for (const approval of snapshot.approvals) validateApprovalRequest(approval)
  for (const subagent of snapshot.subagents) validateSubagentSnapshot(subagent)
  return snapshot as unknown as SessionSnapshot
}

function validateEvent(value: unknown): SessionUpdateEnvelope {
  const event = expectRecord(value, 'session event')
  if (event.schema_version !== SESSION_STREAM_SCHEMA_VERSION) {
    throw new SessionProtocolError(
      `unsupported session stream v${String(event.schema_version)}`,
    )
  }
  if (
    typeof event.stream_id !== 'string' ||
    !isNonNegativeInteger(event.sequence) ||
    !isNonNegativeInteger(event.session_revision) ||
    !isNonNegativeInteger(event.timestamp_ms)
  ) {
    throw new SessionProtocolError('invalid session event')
  }
  validateSessionUpdate(event.update)
  return event as unknown as SessionUpdateEnvelope
}

function validateSessionUpdate(value: unknown): void {
  const update = expectRecord(value, 'session update')
  if (typeof update.type !== 'string' || !('data' in update)) {
    throw new SessionProtocolError('invalid session update')
  }
  switch (update.type) {
    case 'turn_upserted':
      validateTurnProjection(update.data)
      return
    case 'context_replaced':
      validateContextProjection(update.data)
      return
    case 'operation_replaced':
      if (update.data !== null) validateOperationProjection(update.data)
      return
    case 'model_stream_delta': {
      const delta = expectRecord(update.data, 'model stream delta')
      if (
        typeof delta.operation_id !== 'string' ||
        typeof delta.model_call_id !== 'string' ||
        !isOptionalString(delta.text) ||
        !isOptionalString(delta.reasoning)
      ) {
        throw new SessionProtocolError('invalid model stream delta')
      }
      return
    }
    case 'approvals_replaced':
      if (!Array.isArray(update.data)) {
        throw new SessionProtocolError('invalid approval replacement')
      }
      for (const approval of update.data) validateApprovalRequest(approval)
      return
    case 'subagent_upserted':
      validateSubagentSnapshot(update.data)
      return
    case 'subagent_removed': {
      const removed = expectRecord(update.data, 'subagent removal')
      if (typeof removed.instance_id !== 'string') {
        throw new SessionProtocolError('invalid subagent removal')
      }
      return
    }
    case 'middleware_recorded':
      validateMiddlewareInvocation(update.data)
      return
    case 'notice': {
      const notice = expectRecord(update.data, 'session notice')
      if (typeof notice.message !== 'string') {
        throw new SessionProtocolError('invalid session notice')
      }
      return
    }
    default:
      throw new SessionProtocolError(`unsupported session update ${update.type}`)
  }
}

function validateMiddlewareInvocation(value: unknown): void {
  const invocation = expectRecord(value, 'middleware invocation')
  if (
    typeof invocation.invocation_id !== 'string' ||
    typeof invocation.middleware_id !== 'string' ||
    !['internal', 'user_command', 'project_command'].includes(
      String(invocation.source),
    ) ||
    ![
      'before_prompt',
      'before_tool',
      'permission_request',
      'after_tool',
      'pre_compact',
      'post_compact',
    ].includes(String(invocation.stage)) ||
    ![
      'continue',
      'approve',
      'deny',
      'failed_open',
      'failed_closed',
      'cancelled',
      'skipped_untrusted',
    ].includes(String(invocation.outcome)) ||
    !isNonNegativeInteger(invocation.started_at_ms) ||
    !isNonNegativeInteger(invocation.duration_ms) ||
    !isOptionalString(invocation.reason)
  ) {
    throw new SessionProtocolError('invalid middleware invocation')
  }
}

function validateTurnProjection(value: unknown): void {
  const turn = expectRecord(value, 'turn projection')
  if (
    typeof turn.id !== 'string' ||
    typeof turn.operation_id !== 'string' ||
    !isNonNegativeInteger(turn.index) ||
    !['running', 'completed', 'failed', 'cancelled', 'interrupted'].includes(
      String(turn.status),
    ) ||
    !Array.isArray(turn.messages) ||
    !Array.isArray(turn.steps) ||
    !Array.isArray(turn.notices) ||
    !turn.notices.every((notice) => typeof notice === 'string') ||
    !isNonNegativeInteger(turn.started_at_ms) ||
    !isOptionalNonNegativeInteger(turn.completed_at_ms) ||
    !isOptionalString(turn.error)
  ) {
    throw new SessionProtocolError('invalid turn projection')
  }
  validateMessage(turn.user_message)
  validateModelInvocation(turn.model)
  validatePermissionProfile(turn.permissions)
  for (const message of turn.messages) validateMessage(message)
  for (const step of turn.steps) validateSessionStep(step)
}

function validateSessionStep(value: unknown): void {
  const step = expectRecord(value, 'session step')
  if (
    typeof step.id !== 'string' ||
    !['model_call', 'tool_call'].includes(String(step.kind)) ||
    ![
      'running',
      'completed',
      'failed',
      'interrupted',
      'outcome_unknown',
    ].includes(String(step.status)) ||
    !isOptionalString(step.error)
  ) {
    throw new SessionProtocolError('invalid session step')
  }
  if (step.model_message !== undefined && step.model_message !== null) {
    validateMessage(step.model_message)
  }
  if (step.tool_call !== undefined && step.tool_call !== null) {
    validateToolCall(step.tool_call)
  }
  if (step.tool_result !== undefined && step.tool_result !== null) {
    validateMessage(step.tool_result)
  }
  if (step.approval !== undefined && step.approval !== null) {
    validateApprovalRequest(step.approval)
  }
}

function validateContextProjection(value: unknown): void {
  const context = expectRecord(value, 'model context projection')
  if (
    !Array.isArray(context.messages) ||
    !isOptionalString(context.summary) ||
    !isOptionalString(context.covered_through_turn_id) ||
    (context.legacy_checkpoint !== undefined &&
      typeof context.legacy_checkpoint !== 'boolean')
  ) {
    throw new SessionProtocolError('invalid model context projection')
  }
  for (const message of context.messages) validateMessage(message)
}

function validateOperationProjection(value: unknown): void {
  const operation = expectRecord(value, 'operation projection')
  if (
    typeof operation.operation_id !== 'string' ||
    typeof operation.turn_id !== 'string' ||
    typeof operation.phase !== 'string' ||
    typeof operation.cancellable !== 'boolean'
  ) {
    throw new SessionProtocolError('invalid operation projection')
  }
  if (operation.streaming !== undefined && operation.streaming !== null) {
    const streaming = expectRecord(operation.streaming, 'streaming projection')
    if (
      typeof streaming.model_call_id !== 'string' ||
      typeof streaming.content !== 'string' ||
      typeof streaming.reasoning !== 'string'
    ) {
      throw new SessionProtocolError('invalid streaming projection')
    }
  }
}

function validateMessage(value: unknown): void {
  const message = expectRecord(value, 'message')
  if (
    !['system', 'user', 'assistant', 'tool'].includes(String(message.role)) ||
    !isOptionalString(message.content) ||
    !isOptionalString(message.reasoning_content) ||
    !isOptionalString(message.tool_call_id) ||
    (message.tool_calls !== undefined && !Array.isArray(message.tool_calls))
  ) {
    throw new SessionProtocolError('invalid message')
  }
  if (Array.isArray(message.tool_calls)) {
    for (const toolCall of message.tool_calls) validateToolCall(toolCall)
  }
}

function validateToolCall(value: unknown): void {
  const toolCall = expectRecord(value, 'tool call')
  const fn = expectRecord(toolCall.function, 'tool function')
  if (
    typeof toolCall.id !== 'string' ||
    toolCall.type !== 'function' ||
    typeof fn.name !== 'string' ||
    typeof fn.arguments !== 'string'
  ) {
    throw new SessionProtocolError('invalid tool call')
  }
}

function validateModelInvocation(value: unknown): void {
  const model = expectRecord(value, 'model invocation')
  if (
    typeof model.provider_id !== 'string' ||
    typeof model.provider_name !== 'string' ||
    typeof model.model_id !== 'string' ||
    typeof model.model_name !== 'string'
  ) {
    throw new SessionProtocolError('invalid model invocation')
  }
  validateReasoning(model.reasoning)
}

function validateModelSelection(value: unknown): void {
  const selection = expectRecord(value, 'model selection')
  if (
    typeof selection.provider_id !== 'string' ||
    typeof selection.model_id !== 'string'
  ) {
    throw new SessionProtocolError('invalid model selection')
  }
  validateReasoning(selection.reasoning)
}

function validateReasoning(value: unknown): void {
  if (!['off', 'high', 'max'].includes(String(value))) {
    throw new SessionProtocolError('invalid reasoning level')
  }
}

function validatePermissionProfile(value: unknown): void {
  const permissions = expectRecord(value, 'permission profile')
  if (
    !['read_only', 'workspace_write', 'danger_full_access'].includes(
      String(permissions.mode),
    ) ||
    !['deny', 'prompt', 'allow'].includes(String(permissions.shell))
  ) {
    throw new SessionProtocolError('invalid permission profile')
  }
}

function validateApprovalRequest(value: unknown): void {
  const approval = expectRecord(value, 'approval request')
  const action = expectRecord(approval.action, 'approval action')
  if (
    typeof approval.id !== 'string' ||
    typeof approval.reason !== 'string' ||
    !['shell_command', 'file_changes', 'mcp_tool'].includes(String(action.kind))
  ) {
    throw new SessionProtocolError('invalid approval request')
  }
}

function validateSubagentSnapshot(value: unknown): void {
  const subagent = expectRecord(value, 'subagent snapshot')
  const identity = expectRecord(subagent.identity, 'subagent identity')
  if (
    typeof subagent.id !== 'string' ||
    !['explore', 'plan', 'worker', 'reviewer'].includes(String(subagent.role)) ||
    typeof identity.id !== 'string' ||
    typeof identity.name !== 'string' ||
    typeof subagent.status !== 'string' ||
    !isNonNegativeInteger(subagent.created_at_ms) ||
    !isNonNegativeInteger(subagent.updated_at_ms)
  ) {
    throw new SessionProtocolError('invalid subagent snapshot')
  }
}

function isOptionalString(value: unknown): boolean {
  return value === undefined || value === null || typeof value === 'string'
}

function isOptionalNonNegativeInteger(value: unknown): boolean {
  return value === undefined || value === null || isNonNegativeInteger(value)
}

function isNonNegativeInteger(value: unknown): value is number {
  return Number.isSafeInteger(value) && Number(value) >= 0
}

function expectRecord(value: unknown, label: string): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new SessionProtocolError(`invalid ${label}`)
  }
  return value as Record<string, unknown>
}
