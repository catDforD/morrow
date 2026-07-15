import { useEffect, useRef, useState } from 'react'
import {
  ChevronDown,
  Folder,
  FolderOpen,
  HardDrive,
  Laptop,
  LoaderCircle,
  Package,
  RefreshCw,
  Server,
  Terminal,
  X,
} from 'lucide-react'
import {
  connectWsl,
  listWslDistributions,
  listenWslLogs,
  prepareWsl,
  remoteRequest,
} from './desktop'
import type {
  DesktopPlatform,
  DesktopShellState,
  RecentWorkspace,
  RemoteResponse,
  WorkspaceLocation,
  WslDistribution,
  WslProbe,
} from './desktop'

export function WorkspaceMenu({
  name,
  path,
  recentWorkspaces,
  disabled,
  onOpenLocal,
  onOpenRemote,
  onOpenProjects,
  onReconnect,
}: {
  name: string
  path: string
  recentWorkspaces: RecentWorkspace[]
  disabled: boolean
  onOpenLocal: () => void
  onOpenRemote: () => void
  onOpenProjects: () => void
  onReconnect: (index: number) => void
}) {
  const [open, setOpen] = useState(false)
  const rootRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    if (!open) return
    const onPointerDown = (event: PointerEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) setOpen(false)
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false)
    }
    document.addEventListener('pointerdown', onPointerDown)
    document.addEventListener('keydown', onKeyDown)
    return () => {
      document.removeEventListener('pointerdown', onPointerDown)
      document.removeEventListener('keydown', onKeyDown)
    }
  }, [open])

  const run = (action: () => void) => {
    setOpen(false)
    action()
  }

  return (
    <div className={`workspace-picker${open ? ' open' : ''}`} ref={rootRef}>
      <button
        className="workspace-picker-trigger"
        type="button"
        title={path}
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={disabled}
        onClick={() => setOpen((current) => !current)}
      >
        <Folder size={14} />
        <span>{displayProjectName(name)}</span>
        <ChevronDown size={13} />
      </button>

      {open ? (
        <div className="workspace-picker-menu" role="menu">
          <div className="workspace-picker-heading">
            <span>Current project</span>
            <strong>{displayProjectName(name)}</strong>
            <small title={path}>{path}</small>
          </div>

          {recentWorkspaces.slice(0, 4).map((workspace) => (
            <button
              type="button"
              role="menuitem"
              key={`${workspace.target}-${workspace.path}`}
              onClick={() => run(() => onReconnect(workspace.index))}
            >
              {workspace.target === 'Local' ? (
                <Folder size={16} />
              ) : (
                <HardDrive size={16} />
              )}
              <span>
                <strong>{displayProjectName(workspace.label)}</strong>
                <small>{workspace.target}</small>
              </span>
              {workspace.target === 'Local' ? null : <RefreshCw size={14} />}
            </button>
          ))}

          <div className="workspace-picker-separator" />
          <button type="button" role="menuitem" onClick={() => run(onOpenLocal)}>
            <FolderOpen size={16} />
            <span><strong>Open local folder</strong></span>
          </button>
          <button type="button" role="menuitem" onClick={() => run(onOpenRemote)}>
            <Terminal size={16} />
            <span><strong>Remote connection</strong></span>
          </button>
          <button type="button" role="menuitem" onClick={() => run(onOpenProjects)}>
            <Folder size={16} />
            <span><strong>Manage projects</strong></span>
          </button>
        </div>
      ) : null}
    </div>
  )
}

export function ProjectsDialog({
  open,
  state,
  busyIndex,
  onClose,
  onOpenLocal,
  onOpenRemote,
  onReconnect,
}: {
  open: boolean
  state: DesktopShellState | null
  busyIndex: number | null
  onClose: () => void
  onOpenLocal: () => void
  onOpenRemote: () => void
  onReconnect: (index: number) => void
}) {
  if (!open) return null

  return (
    <Modal title="Projects" onClose={onClose} className="projects-dialog">
      <div className="projects-dialog-header">
        <div>
          <p className="eyebrow">Workspaces</p>
          <h2>Choose a project</h2>
          <p>Open a local folder or connect to a remote workspace.</p>
        </div>
        <div className="projects-dialog-actions">
          <button className="secondary" type="button" onClick={onOpenLocal}>
            <FolderOpen size={16} /> Local
          </button>
          <button type="button" onClick={onOpenRemote}>
            <Terminal size={16} /> Remote
          </button>
        </div>
      </div>

      <div className="project-list">
        {state?.recentWorkspaces.length ? (
          state.recentWorkspaces.map((workspace) => {
            const active = isActiveWorkspace(state.activeWorkspace, workspace)
            const busy = busyIndex === workspace.index
            return (
              <button
                className={`project-row${active ? ' active' : ''}`}
                type="button"
                key={`${workspace.target}-${workspace.path}`}
                disabled={busy}
                onClick={() => onReconnect(workspace.index)}
              >
                <span className="project-row-icon">
                  {workspace.target === 'Local' ? (
                    <Folder size={18} />
                  ) : (
                    <HardDrive size={18} />
                  )}
                </span>
                <span className="project-row-copy">
                  <strong>{displayProjectName(workspace.label)}</strong>
                  <small>{workspace.path}</small>
                </span>
                <span className="project-row-target">{workspace.target}</span>
                {busy ? (
                  <LoaderCircle className="spin" size={16} />
                ) : workspace.target === 'Local' ? null : (
                  <RefreshCw size={16} />
                )}
              </button>
            )
          })
        ) : (
          <div className="project-list-empty">
            <Folder size={24} />
            <span>No recent projects yet.</span>
          </div>
        )}
      </div>
    </Modal>
  )
}

export function RemoteConnectionDialog({
  open,
  platform,
  onClose,
}: {
  open: boolean
  platform: DesktopPlatform
  onClose: () => void
}) {
  const [step, setStep] = useState(1)
  const [distributions, setDistributions] = useState<WslDistribution[]>([])
  const [distro, setDistro] = useState('')
  const [user, setUser] = useState('')
  const [probe, setProbe] = useState<WslProbe | null>(null)
  const [path, setPath] = useState('')
  const [listing, setListing] = useState<DirectoryListing | null>(null)
  const [showHidden, setShowHidden] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [logs, setLogs] = useState<string[]>([])

  useEffect(() => {
    if (!open) return
    setStep(1)
    setDistributions([])
    setDistro('')
    setUser('')
    setProbe(null)
    setPath('')
    setListing(null)
    setShowHidden(false)
    setBusy(false)
    setError(null)
    setLogs([])
  }, [open])

  useEffect(() => {
    if (!open) return
    let disposed = false
    let unlisten: (() => void) | undefined
    void listenWslLogs((message) => {
      setLogs((entries) => [...entries.slice(-49), message])
    }).then((stop) => {
      if (disposed) stop()
      else unlisten = stop
    })
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [open])

  if (!open) return null

  const chooseWsl = async () => {
    if (platform !== 'windows') return
    setBusy(true)
    setError(null)
    try {
      const entries = await listWslDistributions()
      if (entries.length === 0) throw new Error('No WSL distributions were found.')
      const supported = entries.find((entry) => entry.version === 2)
      if (!supported) throw new Error('Morrow requires a WSL 2 distribution.')
      setDistributions(entries)
      setDistro(supported.name)
      setStep(2)
    } catch (reason) {
      setError(errorMessage(reason))
    } finally {
      setBusy(false)
    }
  }

  const prepare = async () => {
    setStep(3)
    setBusy(true)
    setError(null)
    setLogs([`Detecting ${distro} environment…`])
    try {
      const nextProbe = await prepareWsl(distro, user)
      setProbe(nextProbe)
      const directory = await loadDirectory(nextProbe.home, showHidden)
      setPath(directory.path)
      setListing(directory)
      setStep(4)
    } catch (reason) {
      const message = errorMessage(reason)
      setError(message)
      setLogs((entries) => [...entries, `Connection failed: ${message}`])
    } finally {
      setBusy(false)
    }
  }

  const loadDirectory = async (nextPath: string, hidden: boolean) => {
    const response = await remoteRequest<Extract<RemoteResponse, { type: 'directory' }>>({
      type: 'list_directory',
      data: { path: nextPath, show_hidden: hidden },
    })
    return response.data
  }

  const browse = async (nextPath: string, hidden = showHidden) => {
    setBusy(true)
    setError(null)
    try {
      const directory = await loadDirectory(nextPath, hidden)
      setPath(directory.path)
      setListing(directory)
    } catch (reason) {
      setError(errorMessage(reason))
    } finally {
      setBusy(false)
    }
  }

  const openWorkspace = async () => {
    if (!probe) return
    setBusy(true)
    setError(null)
    try {
      await connectWsl(distro, probe.user, path)
      window.location.reload()
    } catch (reason) {
      setError(errorMessage(reason))
      setBusy(false)
    }
  }

  const steps = ['Connection type', 'Configuration', 'Connect', 'Directory']

  return (
    <Modal title="Remote connection" onClose={onClose} className="remote-connection-dialog">
      <aside className="remote-connection-steps">
        <strong>Remote connection</strong>
        {steps.map((label, index) => (
          <div className={step === index + 1 ? 'active' : step > index + 1 ? 'done' : ''} key={label}>
            <span>{step > index + 1 ? '✓' : index + 1}</span>
            {label}
          </div>
        ))}
      </aside>

      <section className="remote-connection-content">
        {step === 1 ? (
          <>
            <h2>Choose a connection type</h2>
            <p>Select where the complete Morrow Runtime should execute.</p>
            <div className="remote-target-grid">
              <button type="button" disabled>
                <Server size={22} />
                <strong>SSH</strong>
                <span>Remote host · Coming soon</span>
              </button>
              <button
                type="button"
                disabled={platform !== 'windows' || busy}
                onClick={() => void chooseWsl()}
              >
                <Terminal size={22} />
                <strong>WSL</strong>
                <span>
                  {platform === 'windows'
                    ? 'Windows Subsystem for Linux'
                    : 'Available in the Windows desktop app'}
                </span>
              </button>
              <button type="button" disabled>
                <Package size={22} />
                <strong>Docker</strong>
                <span>Local container · Coming soon</span>
              </button>
            </div>
          </>
        ) : null}

        {step === 2 ? (
          <>
            <h2>Configure WSL</h2>
            <p>Choose a WSL 2 distribution and the Linux account that should run Morrow.</p>
            <label>
              Distribution
              <select value={distro} onChange={(event) => setDistro(event.target.value)}>
                {distributions.map((entry) => (
                  <option key={entry.name} value={entry.name} disabled={entry.version !== 2}>
                    {entry.name}{entry.is_default ? ' · Default' : ''} · WSL {entry.version}
                  </option>
                ))}
              </select>
            </label>
            <label>
              Linux user
              <input
                value={user}
                onChange={(event) => setUser(event.target.value)}
                placeholder="Use distro default user"
              />
            </label>
            <DialogActions>
              <button className="secondary" type="button" onClick={() => setStep(1)}>Back</button>
              <button type="button" disabled={!distro || busy} onClick={() => void prepare()}>
                Connect
              </button>
            </DialogActions>
          </>
        ) : null}

        {step === 3 ? (
          <>
            <h2>Connecting to {distro}</h2>
            <p>Morrow is checking and deploying the matching Linux Runtime.</p>
            <div className="connection-log">
              {logs.map((entry, index) => <div key={`${entry}-${index}`}>{entry}</div>)}
              {busy ? <div><LoaderCircle className="spin" size={15} /> Working…</div> : null}
            </div>
            {!busy && error ? (
              <DialogActions>
                <button className="secondary" type="button" onClick={() => setStep(2)}>Back</button>
              </DialogActions>
            ) : null}
          </>
        ) : null}

        {step === 4 && listing ? (
          <>
            <h2>Choose a Linux project folder</h2>
            <p>{probe?.user}@{distro}</p>
            <div className="remote-path-row">
              <input value={path} onChange={(event) => setPath(event.target.value)} />
              <button type="button" disabled={busy} onClick={() => void browse(path)}>Go</button>
            </div>
            <label className="hidden-toggle">
              <input
                type="checkbox"
                checked={showHidden}
                onChange={(event) => {
                  setShowHidden(event.target.checked)
                  void browse(path, event.target.checked)
                }}
              />
              Show hidden folders
            </label>
            <div className="remote-directory-list">
              {listing.parent ? (
                <button type="button" onClick={() => void browse(listing.parent!)}>
                  <Folder size={16} />..
                </button>
              ) : null}
              {listing.entries.filter((entry) => entry.directory).map((entry) => (
                <button type="button" key={entry.path} onClick={() => void browse(entry.path)}>
                  <Folder size={16} />{entry.name}
                </button>
              ))}
            </div>
            {path.startsWith('/mnt/') ? (
              <p className="connection-warning">
                Projects stored under /mnt may be slower than projects in the WSL filesystem.
              </p>
            ) : null}
            <DialogActions>
              <button className="secondary" type="button" onClick={() => setStep(2)}>Back</button>
              <button type="button" disabled={busy} onClick={() => void openWorkspace()}>
                <Laptop size={16} /> Open project
              </button>
            </DialogActions>
          </>
        ) : null}

        {error ? <div className="connection-error">{error}</div> : null}
      </section>
    </Modal>
  )
}

function Modal({
  title,
  className,
  onClose,
  children,
}: {
  title: string
  className: string
  onClose: () => void
  children: React.ReactNode
}) {
  return (
    <div
      className="workspace-modal-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose()
      }}
    >
      <div className={`workspace-modal ${className}`} role="dialog" aria-modal="true" aria-label={title}>
        <button className="workspace-modal-close" type="button" aria-label="Close" onClick={onClose}>
          <X size={18} />
        </button>
        {children}
      </div>
    </div>
  )
}

function DialogActions({ children }: { children: React.ReactNode }) {
  return <div className="workspace-dialog-actions">{children}</div>
}

function isActiveWorkspace(
  active: WorkspaceLocation | null,
  recent: RecentWorkspace,
): boolean {
  if (!active || active.path !== recent.path) return false
  return active.kind === 'local'
    ? recent.target === 'Local'
    : recent.target === `${active.distro} · WSL`
}

function displayProjectName(name: string): string {
  return name === 'default' ? 'Default project' : name
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

type DirectoryListing = Extract<RemoteResponse, { type: 'directory' }>['data']
