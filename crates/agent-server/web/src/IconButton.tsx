import type { ReactNode } from 'react'

export function IconButton({
  title,
  disabled = false,
  onClick,
  children,
}: {
  title: string
  disabled?: boolean
  onClick: () => void
  children: ReactNode
}) {
  return (
    <button
      className="icon-button"
      type="button"
      title={title}
      disabled={disabled}
      onClick={onClick}
    >
      <span className="sr-only">{title}</span>
      {children}
    </button>
  )
}
export function MiniIconButton({
  title,
  type = 'button',
  disabled = false,
  onClick,
  children,
}: {
  title: string
  type?: 'button' | 'submit'
  disabled?: boolean
  onClick?: () => void
  children: ReactNode
}) {
  return (
    <button
      className="mini-icon-button"
      type={type}
      title={title}
      disabled={disabled}
      onClick={onClick}
    >
      <span className="sr-only">{title}</span>
      {children}
    </button>
  )
}
