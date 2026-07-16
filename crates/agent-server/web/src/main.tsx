import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import DesktopBootstrap from './DesktopBootstrap'
import './styles.css'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <DesktopBootstrap />
  </StrictMode>,
)
