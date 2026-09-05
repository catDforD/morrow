import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { ArrowLeft, ArrowUp, Bot, Check, Copy, MoreHorizontal, Square, Trash2 } from 'lucide-react'
import MarkdownContent from './MarkdownContent'
import type { SubagentInstanceSnapshot, SubagentRunRecord, SubagentRunSummary, SubagentTranscriptSnapshot } from './types'

const roleLabels = { explore: '探索', plan: '规划', worker: '执行', reviewer: '审查' }
const statusLabels = { idle: '空闲', queued: '排队中', running: '执行中', waiting_approval: '等待确认', completed: '已完成', failed: '失败', cancelled: '已取消', interrupted: '已中断' }

export type SubagentInspectorProps = {
  instances: SubagentInstanceSnapshot[]
  transcript: SubagentTranscriptSnapshot | null
  onSend: (instanceId: string, message: string) => void
  onInspect: (instanceId: string) => void
  onCancel: (instanceId: string) => void
  onDelete: (instanceId: string) => void
  commandsEnabled?: boolean
}

export default function SubagentInspector({ instances, transcript, onSend, onInspect, onCancel, onDelete, commandsEnabled = true, renderAvatar }: SubagentInspectorProps & {
  renderAvatar: (instance: SubagentInstanceSnapshot) => ReactNode
}) {
  const [selectedId, setSelectedId] = useState<string | null>(transcript?.instance.id ?? null)
  const [showDetail, setShowDetail] = useState(Boolean(transcript))
  const detailRef = useRef<HTMLHeadingElement | null>(null)
  const listRef = useRef<HTMLDivElement | null>(null)
  const selected = instances.find((instance) => instance.id === selectedId)
  const selectedTranscript = transcript?.instance.id === selectedId ? transcript : null
  useEffect(() => {
    if (!showDetail) return
    detailRef.current?.focus({ preventScroll: true })
  }, [selectedId, showDetail])

  const inspect = (instance: SubagentInstanceSnapshot) => {
    setSelectedId(instance.id)
    setShowDetail(true)
    onInspect(instance.id)
  }
  const back = () => {
    setShowDetail(false)
    requestAnimationFrame(() => listRef.current?.querySelector<HTMLButtonElement>('[aria-pressed="true"]')?.focus())
  }
  return (
    <div className={`persistent-subagent-panel${selected && showDetail ? ' has-selection' : ''}`}>
      <section className="subagent-instance-section" aria-label="子智能体列表">
        <header className="subagent-section-heading"><strong>子智能体</strong><span>{instances.length}</span></header>
        <div className="subagent-instance-list main-scroll" ref={listRef}>
          {!instances.length ? <div className="subagent-empty-state"><Bot size={20} /><span>暂无子智能体</span></div> : null}
          {instances.map((instance) => {
            const data = currentTask(instance, transcript?.instance.id === instance.id ? transcript : null)
            return <button className={`subagent-instance-card${selectedId === instance.id ? ' selected' : ''}`} key={instance.id}
              type="button" aria-pressed={selectedId === instance.id} disabled={!commandsEnabled} onClick={() => inspect(instance)}>
              <span className="subagent-instance-avatar">{renderAvatar(instance)}</span>
              <span className="subagent-instance-content">
                <span className="subagent-instance-heading"><strong>{instance.identity.name}</strong><span className="subagent-role-label">{roleLabels[instance.role]}</span><AgentStatus status={data.status} /></span>
                <span className="subagent-task-preview" title={data.task}>{data.task}</span>
              </span>
            </button>
          })}
        </div>
      </section>
      {selected && showDetail ? <section className="subagent-detail-card" aria-label={`${selected.identity.name}详情`}>
        <button className="subagent-back" type="button" onClick={back}><ArrowLeft size={15} />返回列表</button>
        <header className="subagent-detail-header">
          <span className="subagent-instance-avatar">{renderAvatar(selected)}</span>
          <h3 tabIndex={-1} ref={detailRef}>{selected.identity.name}<span>{roleLabels[selected.role]}</span></h3>
          <AgentStatus status={currentTask(selected, selectedTranscript).status} />
        </header>
        <AgentDetail key={selected.id} instance={selected} transcript={selectedTranscript} onSend={onSend} onCancel={onCancel} onDelete={onDelete} commandsEnabled={commandsEnabled} />
      </section> : null}
    </div>
  )
}

function AgentDetail({ instance, transcript, onSend, onCancel, onDelete, commandsEnabled }: {
  instance: SubagentInstanceSnapshot
  transcript: SubagentTranscriptSnapshot | null
  onSend: SubagentInspectorProps['onSend']
  onCancel: SubagentInspectorProps['onCancel']
  onDelete: SubagentInspectorProps['onDelete']
  commandsEnabled: boolean
}) {
  const [followup, setFollowup] = useState('')
  const { task, status, run, summary } = currentTask(instance, transcript)
  const active = isActive(instance.status)
  const result = summary?.result?.trim()
  const error = summary?.error?.trim()
  return (
    <>
      <div className="subagent-detail-body main-scroll">
        <details className="subagent-task-disclosure" key={instance.latest_run_id ?? 'task'}>
          <summary title={task}><span>{task}</span></summary>
          <div className="subagent-full-task">{task}</div>
        </details>
        <section className="subagent-result" aria-label="任务结果">
          <header><h4>{error ? '错误' : '结果'}</h4>{result ? <CopyResult key={result} content={result} /> : null}</header>
          {error ? <p className="subagent-result-error">{error}</p> : null}
          {result ? <MarkdownContent content={result} className="subagent-result-markdown" /> : !error ? <p className="muted-line" role="status">
            {active ? instance.queue_reason || `${statusLabels[status]}…` : !transcript ? '正在加载结果…' : '暂无结果'}
          </p> : null}
          {summary?.truncated ? <p className="subagent-log-warning">结果已截断</p> : null}
        </section>
        {transcript ? <AgentTechnicalDetails transcript={transcript} instance={instance} summary={summary} run={run} /> : null}
      </div>
      <footer className="subagent-detail-footer">
        {!active ? <form className="subagent-followup-form" onSubmit={(event) => {
          event.preventDefault()
          if (!commandsEnabled || !transcript || !followup.trim()) return
          onSend(instance.id, followup.trim())
          setFollowup('')
        }}>
          <textarea aria-label="后续任务" rows={2} maxLength={4000} value={followup} placeholder="发送后续任务…"
            disabled={!commandsEnabled || !transcript} onChange={(event) => setFollowup(event.target.value)} />
          <button className="send-button composer-primary-button" type="submit" aria-label="发送后续任务" disabled={!commandsEnabled || !transcript || !followup.trim()}><ArrowUp size={16} /></button>
        </form> : <button className="secondary-button subagent-stop" type="button" disabled={!commandsEnabled} onClick={() => onCancel(instance.id)}><Square size={14} />停止任务</button>}
        <details className="subagent-more" onKeyDown={(event) => {
          if (event.key !== 'Escape') return
          event.currentTarget.open = false
          event.currentTarget.querySelector('summary')?.focus()
          event.stopPropagation()
        }} onBlur={(event) => { if (!event.currentTarget.contains(event.relatedTarget)) event.currentTarget.open = false }}>
          <summary aria-label="子智能体操作"><MoreHorizontal size={18} /></summary>
          <div className="subagent-actions-menu">
            <button type="button" disabled={!commandsEnabled || active} onClick={() => {
              if (window.confirm(`删除 ${instance.identity.name} 及其记录？`)) onDelete(instance.id)
            }}><Trash2 size={14} />删除子智能体</button>
          </div>
        </details>
      </footer>
    </>
  )
}

function AgentTechnicalDetails({ transcript, instance, summary, run }: {
  transcript: SubagentTranscriptSnapshot
  instance: SubagentInstanceSnapshot
  summary?: SubagentRunSummary | null
  run?: SubagentRunRecord
}) {
  return <details className="inspector-more subagent-technical-details">
    <summary>详细信息</summary>
    <dl className="inspector-info-list">
      <div><dt>模型</dt><dd>{transcript.model.model_name}</dd></div>
      <div><dt>供应商</dt><dd>{transcript.model.provider_name}</dd></div>
      <div><dt>思考强度</dt><dd>{transcript.model.reasoning}</dd></div>
      <div><dt>权限</dt><dd>{({ read_only: '只读', workspace_write: '工作区写入', danger_full_access: '完全访问' })[transcript.permission_ceiling.mode]}</dd></div>
      <div><dt>Shell 权限</dt><dd>{({ deny: '禁止', prompt: '需确认', allow: '允许' })[transcript.permission_ceiling.shell]}</dd></div>
      <div><dt>超时</dt><dd>{transcript.role_config.timeout_secs} 秒</dd></div>
      <div><dt>工具轮次上限</dt><dd>{transcript.role_config.max_tool_rounds}</dd></div>
      {summary?.model_calls ? <div><dt>模型调用</dt><dd>{summary.model_calls}</dd></div> : null}
      {summary?.tool_calls ? <div><dt>工具调用</dt><dd>{summary.tool_calls}</dd></div> : null}
      {summary?.file_changes.length ? <div><dt>文件变更</dt><dd>{summary.file_changes.map((file) => file.path).join('、')}</dd></div> : null}
      {summary?.shell_commands.length ? <div><dt>执行命令</dt><dd>{summary.shell_commands.map((shell) => shell.command).join('\n')}</dd></div> : null}
      {run ? <div><dt>开始时间</dt><dd>{new Date(run.started_at_ms).toLocaleString()}</dd></div> : null}
      <div><dt>实例状态</dt><dd>{statusLabels[instance.status]}</dd></div>
      <div><dt>实例 ID</dt><dd>{instance.id}</dd></div>
      {instance.latest_run_id ? <div><dt>任务 ID</dt><dd>{instance.latest_run_id}</dd></div> : null}
    </dl>
    {instance.event_log_truncated ? <p className="subagent-log-warning">事件日志已截断</p> : null}
  </details>
}

function AgentStatus({ status }: { status: keyof typeof statusLabels }) {
  return <span className={`agent-status ${status}`} aria-live="polite">{statusLabels[status]}</span>
}

function CopyResult({ content }: { content: string }) {
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle')
  useEffect(() => {
    if (state === 'idle') return
    const timer = window.setTimeout(() => setState('idle'), 2500)
    return () => window.clearTimeout(timer)
  }, [state])
  return <button className="subagent-copy" type="button" aria-label="复制结果" onClick={async () => {
    try { await navigator.clipboard.writeText(content); setState('copied') }
    catch { setState('failed') }
  }}>
    {state === 'copied' ? <Check size={14} /> : <Copy size={14} />}
    <span aria-live="polite">{state === 'copied' ? '已复制' : state === 'failed' ? '复制失败' : '复制'}</span>
  </button>
}

export function currentTask(instance: SubagentInstanceSnapshot, transcript: SubagentTranscriptSnapshot | null) {
  const run = instance.latest_run_id
    ? transcript?.runs.find((item) => item.id === instance.latest_run_id)
    : transcript?.runs.at(-1)
  const candidate = instance.latest_summary && (!instance.latest_run_id || instance.latest_summary.run_id === instance.latest_run_id)
    ? instance.latest_summary
    : run?.summary
  const summary = isActive(instance.status) && candidate && !isActive(candidate.status) ? undefined : candidate
  const status: keyof typeof statusLabels = instance.status === 'idle' ? summary?.status ?? run?.status ?? 'idle' : instance.status
  return { run, summary, status, task: run?.task ?? instance.latest_task ?? '暂无任务' }
}

function isActive(status: keyof typeof statusLabels) {
  return status === 'queued' || status === 'running' || status === 'waiting_approval'
}
