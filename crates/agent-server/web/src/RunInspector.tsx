import type { ReactNode } from 'react'
import { Check, CircleAlert, FileText, RefreshCw, Shield } from 'lucide-react'
import { ExecutionAction, groupExecutionSteps } from './ExecutionTrace'
import type { ApprovalRequest, RunningTurnSnapshot, SessionEntryResponse, TimelineItem } from './types'

export default function RunInspector({ timeline, runningTurn, selectedEntry, approvalQueue, renderApproval }: {
  timeline: TimelineItem[]
  runningTurn: RunningTurnSnapshot | null
  selectedEntry?: SessionEntryResponse
  approvalQueue: ApprovalRequest[]
  renderApproval: (request: ApprovalRequest) => ReactNode
}) {
  const latest = [...timeline].reverse().find((item) => item.kind === 'run')
  const trace = latest?.kind === 'run' ? latest.trace : undefined
  const steps = trace?.steps ?? []
  const failures = steps.filter((step) => step.status === 'error')
  const tools = steps.filter((step) => step.kind !== 'model' && step.status !== 'error')
  const changes = new Map(steps.filter((step) => step.status === 'ok').flatMap((step) =>
    (step.summary?.files ?? []).map((file) => [file.path, file] as const),
  ))
  const waiting = approvalQueue.length > 0
  const failed = !runningTurn && trace?.status === 'failed'
  const label = waiting ? '等待确认' : runningTurn ? '执行中' : failed ? '执行未完成' : trace ? '已完成' : '就绪'
  return (
    <div className="drawer-run">
      <div className={`inspector-status-line${failed ? ' failed' : ''}`} role="status">
        {waiting ? <Shield size={16} /> : runningTurn ? <RefreshCw size={16} className="spinning" /> : failed ? <CircleAlert size={16} /> : <Check size={16} />}
        <strong>{label}</strong>
      </div>
      {waiting ? <section className="inspector-section" aria-label="待确认操作">
        <h3>待确认操作 <span>{approvalQueue.length}</span></h3>
        {approvalQueue.map((request) => <details className="inspector-approval" key={request.id}>
          <summary><Shield size={14} />{request.reason || '操作需要确认'}</summary>
          {renderApproval(request)}
        </details>)}
      </section> : null}
      {failures.length ? <section className="inspector-section" aria-label="失败项">
        <h3>失败项 <span>{failures.length}</span></h3>
        {failures.map((step) => <ExecutionAction key={step.id} steps={[step]} />)}
      </section> : null}
      {changes.size ? <section className="inspector-section" aria-label="修改的文件">
        <h3>修改的文件 <span>{changes.size}</span></h3>
        <ul className="inspector-file-list">{[...changes.values()].map((file) => <li key={file.path}>
          <FileText size={14} /><span title={file.path}>{file.path}</span>
          <small>{({ add: '新增', update: '修改', delete: '删除' })[file.operation]}</small>
        </li>)}</ul>
      </section> : null}
      {tools.length ? <section className="inspector-section" aria-label="工具记录">
        <h3>工具记录 <span>{tools.length}</span></h3>
        {groupExecutionSteps(tools).map((group) => <ExecutionAction key={group.id} steps={group.steps} />)}
      </section> : null}
      {!trace && !waiting ? <p className="muted-line">暂无执行记录</p> : null}
      <details className="inspector-more">
        <summary>详细信息</summary>
        <dl className="inspector-info-list">
          <div><dt>对话轮次</dt><dd>{selectedEntry?.turns ?? 0}</dd></div>
          <div><dt>上下文消息</dt><dd>{selectedEntry?.active_messages ?? 0}</dd></div>
          <div><dt>历史压缩</dt><dd>{selectedEntry?.has_summary ? '已压缩' : '未压缩'}</dd></div>
          {runningTurn ? <div><dt>当前轮次 ID</dt><dd>{runningTurn.turn_id}</dd></div> : null}
        </dl>
      </details>
    </div>
  )
}
