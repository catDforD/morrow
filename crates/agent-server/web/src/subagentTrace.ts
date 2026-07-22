import type { Message, RunStep, SubagentExecutionSummary } from './types'

const delegateTaskTool = 'delegate_task'

export interface SubagentHistoryEntry {
  task: string
  agentName?: string
  summary?: SubagentExecutionSummary
}

export function runningSubagentStep(
  id: string,
  agentName: string | undefined,
  task: string,
): RunStep {
  return {
    id,
    kind: 'subagent',
    status: 'running',
    title: subagentStepTitle(agentName),
    detail: task,
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
  }
}

export function subagentStepTitle(agentName?: string): string {
  const normalized = agentName?.trim()
  return normalized ? `子智能体 · ${normalized}` : '子智能体'
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
        agentName: summary.agent_name,
        summary,
      })
    }
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

function parseSubagentResult(
  content: string | null | undefined,
  fallbackTask: string,
): SubagentExecutionSummary | undefined {
  if (!content) return undefined
  try {
    const value: unknown = JSON.parse(content)
    if (!isRecord(value)) return undefined
    const task = typeof value.task === 'string' ? value.task : fallbackTask
    const agentName = nonEmptyString(value.agent_name)
    const modelCalls = numberOrZero(value.model_calls)
    const toolCalls = numberOrZero(value.tool_calls)
    const truncated = value.truncated === true
    const result = typeof value.result === 'string' ? value.result : undefined
    const error = typeof value.error === 'string' ? value.error : undefined
    return {
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

function numberOrZero(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0
    ? value
    : 0
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
