import { useEffect, useState } from 'react'
import { LoaderCircle } from 'lucide-react'
import App from './App'
import DesktopShell from './DesktopShell'
import { getDesktopPlatform, getDesktopShellState } from './desktop'
import type { DesktopShellState } from './desktop'

export default function DesktopBootstrap() {
  const platform = getDesktopPlatform()
  const [state, setState] = useState<DesktopShellState | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!platform) return
    let disposed = false
    let timer: number | undefined

    const refresh = async () => {
      try {
        const nextState = await getDesktopShellState()
        if (disposed) return
        setState(nextState)
        setError(null)
        if (!nextState.activeWorkspace) {
          timer = window.setTimeout(() => void refresh(), 200)
        }
      } catch (reason) {
        if (disposed) return
        setError(errorMessage(reason))
        timer = window.setTimeout(() => void refresh(), 500)
      }
    }

    void refresh()
    return () => {
      disposed = true
      if (timer !== undefined) window.clearTimeout(timer)
    }
  }, [platform])

  if (!platform || state?.activeWorkspace) return <App />

  return (
    <DesktopShell onOpenAbout={() => undefined}>
      <main className="workspace-startup-page">
        <LoaderCircle className="spin" size={24} />
        <strong>Opening your Morrow workspace…</strong>
        <span>
          {error ?? 'A default workspace will be used until you choose a project.'}
        </span>
      </main>
    </DesktopShell>
  )
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
