import type { ReactNode } from 'react'
import { isValidElement, useEffect, useRef, useState } from 'react'
import { Check, Copy } from 'lucide-react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'

const markdownPlugins = [remarkGfm]

export default function MarkdownContent({ content, className = '' }: {
  content: string
  className?: string
}) {
  return (
    <div className={`markdown-message${className ? ` ${className}` : ''}`}>
      <ReactMarkdown
        remarkPlugins={markdownPlugins}
        skipHtml
        components={{
          a: ({ node: _node, ...props }) => <a {...props} target="_blank" rel="noreferrer" />,
          table: ({ node: _node, ...props }) => (
            <div className="markdown-table-scroll"><table {...props} /></div>
          ),
          pre: ({ children }) => <CodeBlock>{children}</CodeBlock>,
        }}
      >
        {content}
      </ReactMarkdown>
    </div>
  )
}

function CodeBlock({ children }: { children: ReactNode }) {
  const codeRef = useRef<HTMLPreElement | null>(null)
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle')
  const language = isValidElement<{ className?: string }>(children)
    ? children.props.className?.replace(/^language-/, '') : undefined

  useEffect(() => {
    if (copyState === 'idle') return
    const timer = window.setTimeout(() => setCopyState('idle'), 2500)
    return () => window.clearTimeout(timer)
  }, [copyState])

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(codeRef.current?.textContent ?? '')
      setCopyState('copied')
    } catch {
      setCopyState('failed')
    }
  }

  return (
    <div className="markdown-code-block">
      <div className="markdown-code-header">
        <span>{language || '代码'}</span>
        <button type="button" onClick={() => void copy()} aria-label="复制代码">
          {copyState === 'copied' ? <Check size={14} /> : <Copy size={14} />}
          <span aria-live="polite">
            {copyState === 'copied' ? '已复制' : copyState === 'failed' ? '复制失败，请手动选择' : '复制代码'}
          </span>
        </button>
      </div>
      <pre ref={codeRef}>{children}</pre>
    </div>
  )
}
