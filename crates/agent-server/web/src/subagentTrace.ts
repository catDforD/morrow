import type {
  Message,
  RunStep,
  SubagentExecutionSummary,
  SubagentInstanceSnapshot,
  SubagentInstanceStatus,
  SubagentRole,
} from './types'

const delegateTaskTool = 'delegate_task'
const spawnSubagentTool = 'spawn_subagent'

export interface SubagentHistoryEntry {
  task: string
  agentId?: string
  agentName?: string
  summary?: SubagentExecutionSummary
}

export interface PersistentSubagentHistoryEntry {
  task: string
  role?: SubagentRole
  snapshot?: SubagentInstanceSnapshot
}

export function runningSubagentStep(
  id: string,
  agentId: string | undefined,
  agentName: string | undefined,
  task: string,
): RunStep {
  return {
    id,
    kind: 'subagent',
    status: 'running',
    title: subagentStepTitle(agentName),
    detail: task,
    ...(agentId ? { agentId } : {}),
    ...(agentName ? { agentName } : {}),
  }
}

export function finishedSubagentStep(
  id: string,
  ok: boolean,
  summary: SubagentExecutionSummary,
): RunStep {
  return {
    id,
    kind: 'subagent',
    status: ok ? 'ok' : 'error',
    title: subagentStepTitle(summary.agent_name),
    detail: summary.task,
    summary: { subagent: summary },
    ...(summary.agent_id ? { agentId: summary.agent_id } : {}),
    ...(summary.agent_name ? { agentName: summary.agent_name } : {}),
  }
}

export function subagentStepTitle(agentName?: string): string {
  const normalized = agentName?.trim()
  return normalized ? `子智能体 · ${normalized}` : '子智能体'
}

export function startingPersistentSubagentStep(id: string): RunStep {
  return {
    id,
    kind: 'persistent_subagent',
    status: 'running',
    title: '正在启动子 Agent',
    detail: '主 Agent 正在分配身份并创建后台任务。',
  }
}

export function persistentSubagentSnapshotStep(
  id: string,
  snapshot: SubagentInstanceSnapshot,
  task = snapshot.latest_task || '等待主 Agent 分配任务',
): RunStep {
  const failed = ['failed', 'cancelled', 'interrupted'].includes(snapshot.status)
  return {
    id,
    kind: 'persistent_subagent',
    status: failed ? 'error' : 'ok',
    title: `子 Agent · ${snapshot.identity.name}`,
    detail: task,
    agentId: snapshot.identity.id,
    agentName: snapshot.identity.name,
    agentRole: snapshot.role,
    agentStatus: snapshot.status,
    instanceId: snapshot.id,
  }
}

export function failedPersistentSubagentStep(
  id: string,
  error: string | undefined,
  entry?: PersistentSubagentHistoryEntry,
): RunStep {
  return {
    id,
    kind: 'persistent_subagent',
    status: 'error',
    title: '子 Agent 启动失败',
    detail: error || entry?.task || '无法创建后台子 Agent。',
    ...(entry?.role ? { agentRole: entry.role } : {}),
    agentStatus: 'failed',
  }
}

export function subagentHistory(
  messages: Message[],
): Map<string, SubagentHistoryEntry> {
  const entries = new Map<string, SubagentHistoryEntry>()

  for (const message of messages) {
    for (const call of message.tool_calls ?? []) {
      if (call.function.name !== delegateTaskTool) continue
      entries.set(call.id, { task: parseTask(call.function.arguments) })
    }
  }

  for (const message of messages) {
    if (message.role !== 'tool' || !message.tool_call_id) continue
    const entry = entries.get(message.tool_call_id)
    if (!entry) continue
    const summary = parseSubagentResult(message.content, entry.task)
    if (summary) {
      entries.set(message.tool_call_id, {
        task: summary.task,
        ...(summary.agent_id ? { agentId: summary.agent_id } : {}),
        ...(summary.agent_name ? { agentName: summary.agent_name } : {}),
        summary,
      })
    }
  }

  return entries
}

export function persistentSubagentHistory(
  messages: Message[],
): Map<string, PersistentSubagentHistoryEntry> {
  const entries = new Map<string, PersistentSubagentHistoryEntry>()

  for (const message of messages) {
    for (const call of message.tool_calls ?? []) {
      if (call.function.name !== spawnSubagentTool) continue
      entries.set(call.id, parsePersistentSpawnArguments(call.function.arguments))
    }
  }

  for (const message of messages) {
    if (message.role !== 'tool' || !message.tool_call_id) continue
    const entry = entries.get(message.tool_call_id)
    if (!entry) continue
    const snapshot = parsePersistentSubagentSnapshot(message.content)
    if (!snapshot) continue
    entries.set(message.tool_call_id, {
      task: snapshot.latest_task || entry.task,
      role: snapshot.role,
      snapshot,
    })
  }

  return entries
}

function parseTask(argumentsJson: string): string {
  try {
    const value: unknown = JSON.parse(argumentsJson)
    if (isRecord(value) && typeof value.task === 'string' && value.task.trim()) {
      return value.task.trim()
    }
  } catch {
    // The matching tool result will carry the validation error.
  }
  return 'Invalid delegated task'
}

function parsePersistentSpawnArguments(
  argumentsJson: string,
): PersistentSubagentHistoryEntry {
  try {
    const value: unknown = JSON.parse(argumentsJson)
    if (isRecord(value)) {
      return {
        task: nonEmptyString(value.task) ?? '等待主 Agent 分配任务',
        ...(isSubagentRole(value.role) ? { role: value.role } : {}),
      }
    }
  } catch {
    // The matching tool result will carry the validation error.
  }
  return { task: '无法读取子 Agent 任务' }
}

function parsePersistentSubagentSnapshot(
  content: string | null | undefined,
): SubagentInstanceSnapshot | undefined {
  if (!content) return undefined
  try {
    const value: unknown = JSON.parse(content)
    if (!isRecord(value) || !isRecord(value.instance)) return undefined
    const instance = value.instance
    const instanceId = nonEmptyString(instance.id)
    const identity = isRecord(instance.identity) ? instance.identity : undefined
    const identityId = identity ? nonEmptyString(identity.id) : undefined
    const identityName = identity ? nonEmptyString(identity.name) : undefined
    if (
      !instanceId ||
      !isSubagentRole(instance.role) ||
      !identityId ||
      !identityName ||
      !isSubagentInstanceStatus(instance.status) ||
      typeof instance.created_at_ms !== 'number' ||
      typeof instance.updated_at_ms !== 'number'
    ) {
      return undefined
    }

    return {
      id: instanceId,
      role: instance.role,
      identity: {
        id: identityId,
        name: identityName,
      },
      status: instance.status,
      created_at_ms: instance.created_at_ms,
      updated_at_ms: instance.updated_at_ms,
      ...(optionalString(instance.latest_run_id) !== undefined
        ? { latest_run_id: optionalString(instance.latest_run_id) }
        : {}),
      ...(optionalString(instance.latest_task) !== undefined
        ? { latest_task: optionalString(instance.latest_task) }
        : {}),
      ...(optionalString(instance.queue_reason) !== undefined
        ? { queue_reason: optionalString(instance.queue_reason) }
        : {}),
      event_log_truncated: instance.event_log_truncated === true,
    }
  } catch {
    return undefined
  }
}

function parseSubagentResult(
  content: string | null | undefined,
  fallbackTask: string,
): SubagentExecutionSummary | undefined {
  if (!content) return undefined
  try {
    const value: unknown = JSON.parse(content)
    if (!isRecord(value)) return undefined
    const task = typeof value.task === 'string' ? value.task : fallbackTask
    const agentId = nonEmptyString(value.agent_id)
    const agentName = nonEmptyString(value.agent_name)
    const modelCalls = numberOrZero(value.model_calls)
    const toolCalls = numberOrZero(value.tool_calls)
    const truncated = value.truncated === true
    const result = typeof value.result === 'string' ? value.result : undefined
    const error = typeof value.error === 'string' ? value.error : undefined
    return {
      agent_id: agentId,
      agent_name: agentName,
      task,
      result,
      error,
      model_calls: modelCalls,
      tool_calls: toolCalls,
      truncated,
    }
  } catch {
    return undefined
  }
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim() ? value.trim() : undefined
}

function optionalString(value: unknown): string | null | undefined {
  if (value === null) return null
  return typeof value === 'string' ? value : undefined
}

function isSubagentRole(value: unknown): value is SubagentRole {
  return ['explore', 'plan', 'worker', 'reviewer'].includes(String(value))
}

function isSubagentInstanceStatus(
  value: unknown,
): value is SubagentInstanceStatus {
  return [
    'idle',
    'queued',
    'running',
    'waiting_approval',
    'interrupted',
    'failed',
    'cancelled',
  ].includes(String(value))
}

function numberOrZero(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0
    ? value
    : 0
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
