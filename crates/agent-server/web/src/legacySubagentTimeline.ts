import {
  failedPersistentSubagentStep,
  persistentSubagentHistory,
  persistentSubagentSnapshotStep,
  startingPersistentSubagentStep,
  subagentHistory,
  subagentStepTitle,
} from './subagentTrace'
import type {
  Message,
  ModelInvocation,
  ReasoningLevel,
  RunStep,
  RunTrace,
} from './types'

const spawnSubagentToolName = 'spawn_subagent'

export type LegacyTurnStatus = 'running' | 'completed' | 'failed'

export interface LegacyTurnStep {
  kind: 'model_call' | 'tool_call'
  status: LegacyTurnStatus
  tool_name?: string
  tool_call_id?: string
  error?: string | null
}

export interface LegacyTurnRecord {
  turn: {
    status: LegacyTurnStatus
    user_message: Message
    assistant_message?: Message | null
    model?: ModelInvocation | null
    steps: LegacyTurnStep[]
    error?: string | null
  }
  messages: Message[]
}

export interface LegacySubagentSession {
  active_thread: { messages: Message[] }
  turns: LegacyTurnRecord[]
  context: {
    summary?: string
    summarized_turns: number
  }
}

export function historyRunTrace(
  record: LegacyTurnRecord,
  turnIndex: number,
): RunTrace {
  const turn = record.turn
  const subagents = subagentHistory(record.messages)
  const persistentSubagents = persistentSubagentHistory(record.messages)
  const modelMessages = record.messages.filter(
    (message) => message.role === 'assistant',
  )
  let modelMessageIndex = 0
  const steps: RunStep[] = turn.steps.map((step, stepIndex) => {
    const stepId = `history-${turnIndex}-step-${stepIndex}`
    const isSubagent =
      step.kind === 'tool_call' && step.tool_name === 'delegate_task'
    const isPersistentSubagent =
      step.kind === 'tool_call' && step.tool_name === spawnSubagentToolName
    const subagent = step.tool_call_id
      ? subagents.get(step.tool_call_id)
      : undefined
    const persistentSubagent = step.tool_call_id
      ? persistentSubagents.get(step.tool_call_id)
      : undefined
    const modelMessage =
      step.kind === 'model_call'
        ? modelMessages[modelMessageIndex++]
        : undefined

    if (isPersistentSubagent) {
      if (persistentSubagent?.snapshot) {
        const snapshotStep = persistentSubagentSnapshotStep(
          stepId,
          persistentSubagent.snapshot,
        )
        return {
          ...snapshotStep,
          status: step.status === 'failed' ? 'error' : snapshotStep.status,
          detail: step.error || snapshotStep.detail,
        }
      }
      if (step.status === 'failed') {
        return failedPersistentSubagentStep(
          stepId,
          step.error || undefined,
          persistentSubagent,
        )
      }
      return {
        ...startingPersistentSubagentStep(stepId),
        status: step.status === 'completed' ? 'ok' : 'running',
        title:
          step.status === 'completed' ? '子 Agent 已启动' : '正在启动子 Agent',
        detail: persistentSubagent?.task || '正在同步身份、职责和任务状态…',
        ...(persistentSubagent?.role
          ? { agentRole: persistentSubagent.role }
          : {}),
      }
    }

    return {
      id: stepId,
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
      title: isSubagent
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
