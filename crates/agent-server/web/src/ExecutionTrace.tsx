import type { ReactNode } from 'react'
import { useState } from 'react'
import { Check, ChevronRight, CircleAlert, FileText, RefreshCw, Search, Shield, Terminal } from 'lucide-react'
import MarkdownContent from './MarkdownContent'
import type { RunStep, RunTrace } from './types'

type ExecutionGroup = { kind: 'thought' | 'action'; id: string; steps: RunStep[] }

export function groupExecutionSteps(steps: RunStep[]): ExecutionGroup[] {
  const groups: ExecutionGroup[] = []
  for (const step of steps) {
    const previous = groups.at(-1)
    const last = previous?.steps.at(-1)
    const kind = step.kind === 'model' ? 'thought' : 'action'
    const mergeThought = kind === 'thought' && previous?.kind === 'thought'
    const mergeRead = step.kind === 'tool' && step.status === 'ok'
      && last?.kind === 'tool' && last.status === 'ok'
      && ['read_file', 'list_files', 'search_text'].includes(toolName(step))
      && toolName(last) === toolName(step)
    if (previous && (mergeThought || mergeRead)) previous.steps.push(step)
    else groups.push({ kind, id: step.id, steps: [step] })
  }
  return groups
}

export function toolStepLabel(step: RunStep): string {
  const args = toolArguments(step)
  const path = argumentText(args.path) || step.summary?.files?.[0]?.path
  switch (toolName(step)) {
    case 'read_file': return path ? `读取 ${path}` : '读取文件'
    case 'list_files': return path ? `浏览 ${path}` : '浏览文件'
    case 'search_text': return `搜索 ${argumentText(args.pattern) || argumentText(args.query) || path || '文本'}`
    case 'write_file':
    case 'edit_file': return path ? `修改 ${path}` : '修改文件'
    case 'apply_patch': return '应用文件修改'
    case 'shell_command': return `运行 ${argumentText(args.command) || step.summary?.shell?.command || '命令'}`
    case 'web_fetch': return `获取 ${argumentText(args.url) || '网页'}`
    default: return step.kind === 'model' ? '模型调用失败' : step.kind === 'error' ? '执行失败' : step.title
  }
}

export function executionSummary(trace: RunTrace): string {
  const reads = new Set<string>()
  const changes = new Set<string>()
  const failures = trace.steps.filter((step) => step.status === 'error').length
  let commands = 0
  for (const step of trace.steps) {
    if (step.status !== 'ok') continue
    const path = argumentText(toolArguments(step).path)
    if (toolName(step) === 'read_file' && path) reads.add(path)
    for (const file of step.summary?.files ?? []) changes.add(file.path)
    if (toolName(step) === 'shell_command') commands += 1
  }
  return [
    reads.size ? `读取 ${reads.size} 个文件` : '',
    changes.size ? `修改 ${changes.size} 个文件` : '',
    commands ? `运行 ${commands} 条命令` : '',
    failures ? `${failures} 次调用失败` : '',
  ].filter(Boolean).join(' · ') || (trace.toolCount ? `${trace.toolCount} 次工具调用` : '')
}

export default function ExecutionTrace({ trace, onToggle, renderSpecialStep }: {
  trace: RunTrace
  onToggle: () => void
  renderSpecialStep: (step: RunStep) => ReactNode
}) {
  const summary = executionSummary(trace)
  const status = trace.status === 'completed' ? 'ok' : trace.status === 'failed' ? 'error' : trace.status
  const title = trace.status === 'completed' ? '已完成'
    : trace.status === 'failed' ? '执行失败'
    : trace.status === 'approval' ? '等待确认'
    : trace.steps.at(-1)?.kind === 'model' ? '正在思考' : '正在执行'
  return (
    <section className={`execution-trace ${trace.status}`} aria-label="执行过程">
      <button className="execution-toggle" type="button" aria-expanded={!trace.collapsed} onClick={onToggle}>
        <ExecutionStatus status={status} />
        <span aria-live="polite">{title}</span>
        {summary ? <span className="execution-summary">· {summary}</span> : null}
        <ChevronRight size={14} className={!trace.collapsed ? 'expanded-chevron' : undefined} />
      </button>
      {!trace.collapsed ? <ExecutionSteps steps={trace.steps} renderSpecialStep={renderSpecialStep} /> : null}
    </section>
  )
}

function ExecutionSteps({ steps, renderSpecialStep }: {
  steps: RunStep[]
  renderSpecialStep: (step: RunStep) => ReactNode
}) {
  const [expandedSteps, setExpandedSteps] = useState<Record<string, boolean>>({})
  const toggleGroup = (group: ExecutionGroup, open: boolean) => {
    setExpandedSteps((current) => {
      if (group.steps.every((step) => Boolean(current[step.id]) === open)) return current
      return { ...current, ...Object.fromEntries(group.steps.map((step) => [step.id, open])) }
    })
  }
  return (
    <div className="execution-stream">
      {groupExecutionSteps(steps).map((group) => {
        const step = group.steps[0]
        if (group.kind === 'thought') return <ThoughtGroup key={group.id} steps={group.steps} />
        if (step.kind === 'subagent' || step.kind === 'persistent_subagent') {
          return <div key={group.id} className="execution-special">{renderSpecialStep(step)}</div>
        }
        return <ExecutionAction
          key={group.id}
          steps={group.steps}
          open={group.steps.some((step) => expandedSteps[step.id])}
          onToggle={(open) => toggleGroup(group, open)}
        />
      })}
    </div>
  )
}

function ThoughtGroup({ steps }: { steps: RunStep[] }) {
  const content = steps.flatMap((step) => [step.reasoning, step.commentary]).filter(Boolean).join('\n\n')
  return (
    <>
      {content ? <MarkdownContent content={content} className="execution-thought" /> : null}
      {steps.filter((step) => step.status === 'error').map((step) => <ExecutionAction key={step.id} steps={[step]} />)}
    </>
  )
}

export function ExecutionAction({ steps, open, onToggle }: {
  steps: RunStep[]
  open?: boolean
  onToggle?: (open: boolean) => void
}) {
  const step = steps[0]
  const label = steps.length === 1 ? toolStepLabel(step) : groupedToolLabel(steps)
  return (
    <details className={`execution-action ${step.status}`} open={open} onToggle={(event) => onToggle?.(event.currentTarget.open)}>
      <summary title={label}>
        <ExecutionStatus status={step.status} step={step} />
        <span className="execution-action-label">{label}</span>
        {step.status === 'running' ? <span className="sr-only">执行中</span> : null}
        {step.status === 'error' ? <span className="execution-action-status">失败</span> : null}
        {step.status === 'approval' ? <span className="execution-action-status">待确认</span> : null}
        <ChevronRight size={13} className="execution-action-chevron" />
      </summary>
      <div className="execution-action-details">
        {steps.map((item) => <ExecutionDetails key={item.id} step={item} />)}
      </div>
    </details>
  )
}

function ExecutionDetails({ step }: { step: RunStep }) {
  const summary = step.summary
  return (
    <div className="execution-detail">
      <strong>{step.toolCall?.function.name || step.title}</strong>
      {step.toolCall ? <pre aria-label="调用参数">{prettyArguments(step.toolCall.function.arguments)}</pre> : null}
      {summary?.files?.length ? <ul>{summary.files.map((file) => (
        <li key={`${file.operation}-${file.path}`}>{({ add: '新增', update: '修改', delete: '删除' })[file.operation]} {file.path}</li>
      ))}</ul> : null}
      {summary?.shell ? <div className="execution-shell-result">
        <code>{summary.shell.command}</code>
        <span>退出码：{summary.shell.exit_code ?? '未知'}</span>
        {summary.shell.timed_out ? <span>已超时</span> : null}
        {summary.shell.stdout_truncated || summary.shell.stderr_truncated ? <span>输出已截断</span> : null}
      </div> : null}
      {step.output ? <pre aria-label="工具输出">{step.output}</pre> : null}
      {summary?.diff ? <pre aria-label="文件差异">{summary.diff}</pre> : null}
      {step.status === 'error' ? <pre className="execution-error">{summary?.error || step.detail || '执行失败'}</pre> : null}
      {!step.toolCall && step.status !== 'error' && step.detail ? <p>{step.detail}</p> : null}
    </div>
  )
}

function ExecutionStatus({ status, step }: { status: RunStep['status']; step?: RunStep }) {
  if (status === 'running') return <RefreshCw size={14} className="spinning" aria-hidden="true" />
  if (status === 'error') return <CircleAlert size={14} aria-hidden="true" />
  if (status === 'approval') return <Shield size={14} aria-hidden="true" />
  if (step && toolName(step) === 'search_text') return <Search size={14} aria-hidden="true" />
  if (step && toolName(step) === 'shell_command') return <Terminal size={14} aria-hidden="true" />
  if (step && ['read_file', 'list_files'].includes(toolName(step))) return <FileText size={14} aria-hidden="true" />
  return <Check size={14} aria-hidden="true" />
}

function groupedToolLabel(steps: RunStep[]): string {
  const name = toolName(steps[0])
  const verb = name === 'read_file' ? '读取' : name === 'list_files' ? '浏览' : '搜索'
  const targets = steps.map((step) => toolStepLabel(step).replace(/^(读取|浏览|搜索)\s*/, ''))
  return `${verb} ${targets.join('、')}`
}

function toolName(step: RunStep): string {
  return step.toolCall?.function.name || step.title
}

function toolArguments(step: RunStep): Record<string, unknown> {
  try {
    const value: unknown = JSON.parse(step.toolCall?.function.arguments ?? '{}')
    return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : {}
  } catch {
    return {}
  }
}

function argumentText(value: unknown): string {
  return typeof value === 'string' ? value : ''
}

function prettyArguments(value: string): string {
  try { return JSON.stringify(JSON.parse(value), null, 2) }
  catch { return value }
}
