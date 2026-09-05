import { ChevronRight, CircleAlert, Info } from 'lucide-react'
import type { TimelineItem, TimelineNoticeItem } from './types'

type NoticeGroup = { kind: 'notices'; id: string; notices: TimelineNoticeItem[] }

export function groupTimelineNotices(items: TimelineItem[]): Array<Exclude<TimelineItem, TimelineNoticeItem> | NoticeGroup> {
  const groups: Array<Exclude<TimelineItem, TimelineNoticeItem> | NoticeGroup> = []
  for (const item of items) {
    if (item.kind !== 'notice') {
      groups.push(item)
      continue
    }
    const last = groups.at(-1)
    if (last?.kind === 'notices') last.notices.push(item)
    else groups.push({ kind: 'notices', id: item.id, notices: [item] })
  }
  return groups
}

export default function TimelineNotices({ notices }: { notices: TimelineNoticeItem[] }) {
  const connections = notices.filter(isConnectionError).length
  const others = notices.length - connections
  const hasError = connections > 0 || notices.some((notice) => notice.tone === 'error')
  const label = [
    connections ? `${connections} 项连接异常` : '',
    others ? `${others} 项${notices.some((notice) => notice.tone === 'error') ? '异常' : '提示'}` : '',
  ].filter(Boolean).join(' · ')
  return (
    <details className={`execution-notices${hasError ? ' warning' : ''}`}>
      <summary>
        {hasError ? <CircleAlert size={14} /> : <Info size={14} />}
        <span>{label}</span>
        <ChevronRight size={13} className="execution-action-chevron" />
      </summary>
      <div className="execution-notice-details">
        {notices.map((notice) => <div key={notice.id}>
          {notice.title !== 'Notice' ? <strong>{notice.title}</strong> : null}
          {notice.detail ? <pre>{notice.detail}</pre> : null}
        </div>)}
      </div>
    </details>
  )
}

function isConnectionError(notice: TimelineNoticeItem): boolean {
  return /^mcp server\b/i.test(notice.detail ?? '')
    && /failed|error|closed|timed? out|unavailable/i.test(notice.detail ?? '')
}
