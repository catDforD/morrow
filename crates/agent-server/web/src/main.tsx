import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import AppErrorBoundary from './AppErrorBoundary'
import DesktopBootstrap from './DesktopBootstrap'
import './styles.css'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <AppErrorBoundary>
      <DesktopBootstrap />
    </AppErrorBoundary>
  </StrictMode>,
)
