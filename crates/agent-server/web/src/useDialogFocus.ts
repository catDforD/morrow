import type { RefObject } from 'react'
import { useEffect } from 'react'

export function useDialogFocus(
  ref: RefObject<HTMLElement | null>,
  activeKey: string | null,
  responsive = false,
) {
  useEffect(() => {
    const panel = ref.current
    if (activeKey === null || !panel) return
    const previous = document.activeElement
    panel.focus({ preventScroll: true })

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Tab') return
      if (responsive && window.matchMedia('(min-width: 1200px)').matches) return
      const controls = Array.from(panel.querySelectorAll<HTMLElement>(
        'button:not(:disabled), a[href], input:not(:disabled), textarea:not(:disabled), select:not(:disabled), summary, [tabindex="0"]',
      )).filter((element) => {
        if (element.closest('[inert], [hidden]')) return false
        const closedDetails = element.closest('details:not([open])')
        return !closedDetails || element === closedDetails.querySelector('summary')
      })
      const first = controls[0]
      const last = controls.at(-1)
      if (!first || !last) {
        event.preventDefault()
        return
      }
      if (event.shiftKey && (document.activeElement === first || document.activeElement === panel)) {
        event.preventDefault()
        last.focus()
      } else if (!event.shiftKey && (document.activeElement === last || document.activeElement === panel)) {
        event.preventDefault()
        first.focus()
      }
    }

    panel.addEventListener('keydown', onKeyDown)
    return () => {
      panel.removeEventListener('keydown', onKeyDown)
      if (previous instanceof HTMLElement && previous.isConnected && !previous.closest('[inert]')) {
        previous.focus({ preventScroll: true })
      }
    }
  }, [activeKey, ref, responsive])
}
