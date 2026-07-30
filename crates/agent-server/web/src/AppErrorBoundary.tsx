import { Component } from 'react'
import type { ErrorInfo, ReactNode } from 'react'

interface AppErrorBoundaryProps {
  children: ReactNode
}

interface AppErrorBoundaryState {
  error: Error | null
}

export default class AppErrorBoundary extends Component<
  AppErrorBoundaryProps,
  AppErrorBoundaryState
> {
  state: AppErrorBoundaryState = { error: null }

  static getDerivedStateFromError(error: Error): AppErrorBoundaryState {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error('Morrow Web crashed', error, info)
  }

  render(): ReactNode {
    if (!this.state.error) return this.props.children
    return (
      <main className="app-error-boundary" role="alert">
        <section>
          <h1>Morrow could not continue</h1>
          <p>{this.state.error.message || 'An unexpected UI error occurred.'}</p>
          <button type="button" onClick={() => location.reload()}>
            Reload application
          </button>
        </section>
      </main>
    )
  }
}
