import type { ReactNode } from 'react'

type TooltipButtonProps = {
  label: string
  onClick?: () => void
  children: ReactNode
  active?: boolean
  className?: string
  ariaExpanded?: boolean
  disabled?: boolean
}

export function TooltipButton({ label, onClick, children, active = false, className = '', ariaExpanded, disabled = false }: TooltipButtonProps) {
  return (
    <button className={`icon-button ${active ? 'is-active' : ''} ${className}`} onClick={onClick} aria-label={label} aria-expanded={ariaExpanded} title={label} type="button" disabled={disabled}>
      {children}
    </button>
  )
}
