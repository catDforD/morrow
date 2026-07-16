import type {
  CSSProperties,
  KeyboardEvent as ReactKeyboardEvent,
  MouseEvent as ReactMouseEvent,
  ReactNode,
} from 'react'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { ChevronRight, Copy, Minus, Square, X } from 'lucide-react'
import {
  captureEditingContext,
  executeEditCommand,
  getDesktopPlatform,
  getDesktopShellState,
  restoreEditingContext,
  runDesktopAction,
} from './desktop'
import type {
  DesktopAction,
  DesktopPlatform,
  DesktopShellState,
  EditCommand,
  EditingContext,
  RecentWorkspace,
} from './desktop'

type MenuId = 'file' | 'edit' | 'window' | 'help'
type MenuEntry = MenuCommand | MenuSeparator | MenuSubmenu

interface MenuCommand {
  type: 'command'
  id: string
  label: string
  accelerator?: string
  disabled?: boolean
  action: () => void | Promise<void>
}

interface MenuSeparator {
  type: 'separator'
  id: string
}

interface MenuSubmenu {
  type: 'submenu'
  id: string
  label: string
  entries: MenuCommand[]
}

const menuOrder: MenuId[] = ['file', 'edit', 'window', 'help']
const menuLabels: Record<MenuId, string> = {
  file: 'File',
  edit: 'Edit',
  window: 'Window',
  help: 'Help',
}

const initialShellState: DesktopShellState = {
  isMaximized: false,
  recentWorkspaces: [],
  activeWorkspace: null,
}

const desktopShellStyle = {
  '--desktop-titlebar-height': '40px',
} as CSSProperties

export default function DesktopShell({
  onOpenAbout,
  children,
}: {
  onOpenAbout: () => void
  children: ReactNode
}) {
  const platform = getDesktopPlatform()
  if (!platform) return <>{children}</>

  return (
    <div
      className={`desktop-shell desktop-${platform}`}
      style={desktopShellStyle}
    >
      <DesktopTitleBar platform={platform} onOpenAbout={onOpenAbout} />
      <div className="desktop-shell-content">{children}</div>
    </div>
  )
}

function DesktopTitleBar({
  platform,
  onOpenAbout,
}: {
  platform: DesktopPlatform
  onOpenAbout: () => void
}) {
  const [shellState, setShellState] = useState(initialShellState)
  const refreshState = useCallback(async () => {
    try {
      setShellState(await getDesktopShellState())
    } catch (error) {
      console.error('Could not read the desktop shell state', error)
    }
  }, [])

  useEffect(() => {
    void refreshState()
    let frame = 0
    const handleResize = () => {
      cancelAnimationFrame(frame)
      frame = requestAnimationFrame(() => void refreshState())
    }
    window.addEventListener('resize', handleResize)
    return () => {
      cancelAnimationFrame(frame)
      window.removeEventListener('resize', handleResize)
    }
  }, [refreshState])

  const runAction = useCallback(
    async (action: DesktopAction) => {
      try {
        await runDesktopAction(action)
        if (action.type === 'toggle_maximize') await refreshState()
      } catch (error) {
        console.error(`Desktop action ${action.type} failed`, error)
      }
    },
    [refreshState],
  )

  const handleTitleBarMouseDown = (event: ReactMouseEvent<HTMLElement>) => {
    if (event.button !== 0 || isInteractiveTarget(event.target)) return
    event.preventDefault()
    if (event.detail === 2) {
      void runAction({ type: 'toggle_maximize' })
    } else if (event.detail === 1) {
      void runAction({ type: 'start_drag' })
    }
  }

  return (
    <header
      className={`desktop-titlebar ${platform}`}
      onMouseDown={handleTitleBarMouseDown}
    >
      {platform !== 'macos' ? (
        <>
          <DesktopMenuBar
            state={shellState}
            runAction={runAction}
            onOpenAbout={onOpenAbout}
          />
          <div className="desktop-titlebar-drag-region" aria-hidden="true" />
          <WindowControls state={shellState} runAction={runAction} />
        </>
      ) : (
        <div className="desktop-titlebar-drag-region" aria-hidden="true" />
      )}
    </header>
  )
}

function DesktopMenuBar({
  state,
  runAction,
  onOpenAbout,
}: {
  state: DesktopShellState
  runAction: (action: DesktopAction) => Promise<void>
  onOpenAbout: () => void
}) {
  const [openMenu, setOpenMenu] = useState<MenuId | null>(null)
  const [focusedMenu, setFocusedMenu] = useState<MenuId>('file')
  const [submenuOpen, setSubmenuOpen] = useState(false)
  const menuBarRef = useRef<HTMLElement | null>(null)
  const triggerRefs = useRef<Record<MenuId, HTMLButtonElement | null>>({
    file: null,
    edit: null,
    window: null,
    help: null,
  })
  const editingContextRef = useRef<EditingContext | null>(null)
  const altPressedRef = useRef(false)
  const altUsedRef = useRef(false)

  const rememberEditingContext = useCallback(() => {
    if (!editingContextRef.current) {
      editingContextRef.current = captureEditingContext()
    }
  }, [])

  const closeMenus = useCallback((restoreEditor = false) => {
    setOpenMenu(null)
    setSubmenuOpen(false)
    if (restoreEditor && editingContextRef.current) {
      const context = editingContextRef.current
      requestAnimationFrame(() => restoreEditingContext(context))
    }
    editingContextRef.current = null
  }, [])

  const openMenuAndFocus = useCallback(
    (menu: MenuId, focusFirstItem: boolean) => {
      rememberEditingContext()
      setFocusedMenu(menu)
      setOpenMenu(menu)
      setSubmenuOpen(false)
      requestAnimationFrame(() => {
        if (focusFirstItem) {
          menuBarRef.current
            ?.querySelector<HTMLElement>(`#desktop-menu-${menu} [role="menuitem"]:not([aria-disabled="true"])`)
            ?.focus()
        }
      })
    },
    [rememberEditingContext],
  )

  const moveTopLevel = useCallback(
    (direction: 1 | -1, keepOpen: boolean) => {
      const current = menuOrder.indexOf(focusedMenu)
      const next = menuOrder[(current + direction + menuOrder.length) % menuOrder.length]
      setFocusedMenu(next)
      setSubmenuOpen(false)
      if (keepOpen) setOpenMenu(next)
      requestAnimationFrame(() => {
        if (keepOpen) {
          menuBarRef.current
            ?.querySelector<HTMLElement>(
              `#desktop-menu-${next} [role="menuitem"]:not([aria-disabled="true"])`,
            )
            ?.focus()
        } else {
          triggerRefs.current[next]?.focus()
        }
      })
    },
    [focusedMenu],
  )

  useEffect(() => {
    const handlePointerDown = (event: PointerEvent) => {
      if (!menuBarRef.current?.contains(event.target as Node)) closeMenus()
    }
    const handleBlur = () => closeMenus()
    window.addEventListener('pointerdown', handlePointerDown)
    window.addEventListener('blur', handleBlur)
    return () => {
      window.removeEventListener('pointerdown', handlePointerDown)
      window.removeEventListener('blur', handleBlur)
    }
  }, [closeMenus])

  useEffect(() => {
    const handleKeyDown = (event: globalThis.KeyboardEvent) => {
      const key = event.key.toLowerCase()
      const menuByAccessKey: Partial<Record<string, MenuId>> = {
        f: 'file',
        e: 'edit',
        w: 'window',
        h: 'help',
      }

      if (event.key === 'Alt') {
        altPressedRef.current = true
        altUsedRef.current = false
        return
      }
      if (altPressedRef.current) altUsedRef.current = true
      if (
        event.altKey &&
        !event.ctrlKey &&
        !event.metaKey &&
        menuByAccessKey[key]
      ) {
        event.preventDefault()
        openMenuAndFocus(menuByAccessKey[key]!, true)
        return
      }
      if (
        event.key === 'F10' &&
        !event.shiftKey &&
        !event.altKey &&
        !event.ctrlKey &&
        !event.metaKey
      ) {
        event.preventDefault()
        rememberEditingContext()
        setFocusedMenu('file')
        setOpenMenu(null)
        triggerRefs.current.file?.focus()
        return
      }
      if ((event.ctrlKey || event.metaKey) && !event.altKey) {
        if (key === 'o') {
          event.preventDefault()
          void runAction({ type: 'open_folder' })
        } else if (key === 'w') {
          event.preventDefault()
          void runAction({ type: 'close_window' })
        }
      }
    }
    const handleKeyUp = (event: globalThis.KeyboardEvent) => {
      if (event.key !== 'Alt') return
      if (altPressedRef.current && !altUsedRef.current) {
        event.preventDefault()
        rememberEditingContext()
        setFocusedMenu('file')
        setOpenMenu(null)
        triggerRefs.current.file?.focus()
      }
      altPressedRef.current = false
      altUsedRef.current = false
    }
    window.addEventListener('keydown', handleKeyDown)
    window.addEventListener('keyup', handleKeyUp)
    return () => {
      window.removeEventListener('keydown', handleKeyDown)
      window.removeEventListener('keyup', handleKeyUp)
    }
  }, [openMenuAndFocus, rememberEditingContext, runAction])

  const runEdit = useCallback(async (command: EditCommand) => {
    const context = editingContextRef.current ?? captureEditingContext()
    closeMenus()
    await executeEditCommand(command, context)
  }, [closeMenus])

  const invokeAndClose = useCallback(
    (action: DesktopAction) => {
      closeMenus()
      void runAction(action)
    },
    [closeMenus, runAction],
  )

  const menuEntries = useMemo<Record<MenuId, MenuEntry[]>>(
    () => ({
      file: [
        command('file.open-folder', 'Open Folder…', 'Ctrl+O', () =>
          invokeAndClose({ type: 'open_folder' }),
        ),
        {
          type: 'submenu',
          id: 'file.open-recent',
          label: 'Open Recent',
          entries:
            state.recentWorkspaces.length > 0
              ? state.recentWorkspaces.map((workspace) =>
                  recentCommand(workspace, invokeAndClose),
                )
              : [
                  command(
                    'file.open-recent.empty',
                    'No Recent Folders',
                    undefined,
                    () => undefined,
                    true,
                  ),
                ],
        },
        separator('file.separator'),
        command('file.close', 'Close Window', 'Ctrl+W', () =>
          invokeAndClose({ type: 'close_window' }),
        ),
        command('file.quit', 'Quit Morrow', undefined, () =>
          invokeAndClose({ type: 'quit' }),
        ),
      ],
      edit: [
        command('edit.undo', 'Undo', 'Ctrl+Z', () => void runEdit('undo')),
        command('edit.redo', 'Redo', 'Ctrl+Y', () => void runEdit('redo')),
        separator('edit.separator.history'),
        command('edit.cut', 'Cut', 'Ctrl+X', () => void runEdit('cut')),
        command('edit.copy', 'Copy', 'Ctrl+C', () => void runEdit('copy')),
        command('edit.paste', 'Paste', 'Ctrl+V', () => void runEdit('paste')),
        separator('edit.separator.selection'),
        command('edit.select-all', 'Select All', 'Ctrl+A', () =>
          void runEdit('select_all'),
        ),
      ],
      window: [
        command('window.minimize', 'Minimize', undefined, () =>
          invokeAndClose({ type: 'minimize' }),
        ),
        command(
          'window.maximize',
          state.isMaximized ? 'Restore' : 'Maximize',
          undefined,
          () => invokeAndClose({ type: 'toggle_maximize' }),
        ),
        separator('window.separator'),
        command('window.close', 'Close Window', 'Ctrl+W', () =>
          invokeAndClose({ type: 'close_window' }),
        ),
      ],
      help: [
        command(
          'help.download',
          'Download Latest Version',
          undefined,
          () => invokeAndClose({ type: 'download_latest' }),
        ),
        command('help.logs', 'Open Logs', undefined, () =>
          invokeAndClose({ type: 'open_logs' }),
        ),
        separator('help.separator'),
        command('help.about', 'About Morrow', undefined, () => {
          closeMenus()
          onOpenAbout()
        }),
      ],
    }),
    [
      closeMenus,
      invokeAndClose,
      onOpenAbout,
      runEdit,
      state.isMaximized,
      state.recentWorkspaces,
    ],
  )

  const handleMenuBarKeyDown = (event: ReactKeyboardEvent<HTMLElement>) => {
    if (event.defaultPrevented) return
    if (event.key === 'ArrowRight' || event.key === 'ArrowLeft') {
      event.preventDefault()
      moveTopLevel(event.key === 'ArrowRight' ? 1 : -1, openMenu !== null)
      return
    }
    if (event.key === 'Escape') {
      event.preventDefault()
      closeMenus(true)
    }
  }

  return (
    <nav
      ref={menuBarRef}
      className="desktop-menu-bar"
      role="menubar"
      aria-label="Application menu"
      onKeyDown={handleMenuBarKeyDown}
    >
      {menuOrder.map((menu) => (
        <div className="desktop-menu-root" key={menu}>
          <button
            ref={(element) => {
              triggerRefs.current[menu] = element
            }}
            className={`desktop-menu-trigger${openMenu === menu ? ' active' : ''}`}
            type="button"
            role="menuitem"
            tabIndex={focusedMenu === menu ? 0 : -1}
            aria-haspopup="menu"
            aria-expanded={openMenu === menu}
            aria-controls={`desktop-menu-${menu}`}
            onPointerDown={(event) => {
              rememberEditingContext()
              event.preventDefault()
            }}
            onClick={() => {
              if (openMenu === menu) closeMenus(true)
              else openMenuAndFocus(menu, false)
            }}
            onMouseEnter={() => {
              if (openMenu && openMenu !== menu) openMenuAndFocus(menu, false)
            }}
            onKeyDown={(event) => {
              if (
                event.key === 'ArrowDown' ||
                event.key === 'Enter' ||
                event.key === ' '
              ) {
                event.preventDefault()
                openMenuAndFocus(menu, true)
              }
            }}
          >
            {menuLabels[menu]}
          </button>
          {openMenu === menu ? (
            <DesktopMenu
              id={`desktop-menu-${menu}`}
              entries={menuEntries[menu]}
              submenuOpen={submenuOpen}
              setSubmenuOpen={setSubmenuOpen}
              onClose={() => closeMenus(true)}
              onMoveTopLevel={moveTopLevel}
              triggerRef={triggerRefs.current[menu]}
            />
          ) : null}
        </div>
      ))}
    </nav>
  )
}

function DesktopMenu({
  id,
  entries,
  submenuOpen,
  setSubmenuOpen,
  onClose,
  onMoveTopLevel,
  triggerRef,
}: {
  id: string
  entries: MenuEntry[]
  submenuOpen: boolean
  setSubmenuOpen: (open: boolean) => void
  onClose: () => void
  onMoveTopLevel: (direction: 1 | -1, keepOpen: boolean) => void
  triggerRef: HTMLButtonElement | null
}) {
  const menuRef = useRef<HTMLDivElement | null>(null)
  const submenuTriggerRef = useRef<HTMLButtonElement | null>(null)

  const handleKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement
    const currentMenu = target.closest<HTMLElement>('[role="menu"]')
    if (!currentMenu) return
    const items = enabledMenuItems(currentMenu)
    const index = items.indexOf(target)

    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault()
      event.stopPropagation()
      const direction = event.key === 'ArrowDown' ? 1 : -1
      items[(index + direction + items.length) % items.length]?.focus()
      return
    }
    if (event.key === 'Home' || event.key === 'End') {
      event.preventDefault()
      event.stopPropagation()
      items[event.key === 'Home' ? 0 : items.length - 1]?.focus()
      return
    }
    if (
      target.dataset.submenuTrigger === 'true' &&
      (event.key === 'ArrowRight' ||
        event.key === 'Enter' ||
        event.key === ' ')
    ) {
      event.preventDefault()
      event.stopPropagation()
      setSubmenuOpen(true)
      requestAnimationFrame(() =>
        menuRef.current
          ?.querySelector<HTMLElement>(
            '.desktop-submenu [role="menuitem"]:not([aria-disabled="true"])',
          )
          ?.focus(),
      )
      return
    }
    if (event.key === 'ArrowRight') {
      if (!target.closest('.desktop-submenu')) {
        event.preventDefault()
        event.stopPropagation()
        onMoveTopLevel(1, true)
      }
      return
    }
    if (event.key === 'ArrowLeft') {
      if (target.closest('.desktop-submenu')) {
        event.preventDefault()
        event.stopPropagation()
        setSubmenuOpen(false)
        submenuTriggerRef.current?.focus()
      } else {
        event.preventDefault()
        event.stopPropagation()
        onMoveTopLevel(-1, true)
      }
      return
    }
    if (event.key === 'Escape') {
      event.preventDefault()
      event.stopPropagation()
      if (target.closest('.desktop-submenu')) {
        setSubmenuOpen(false)
        submenuTriggerRef.current?.focus()
      } else {
        onClose()
        triggerRef?.focus()
      }
    }
  }

  return (
    <div
      ref={menuRef}
      id={id}
      className="desktop-menu"
      role="menu"
      onKeyDown={handleKeyDown}
      onPointerDown={(event) => event.preventDefault()}
    >
      {entries.map((entry) => {
        if (entry.type === 'separator') {
          return <div className="desktop-menu-separator" role="separator" key={entry.id} />
        }
        if (entry.type === 'submenu') {
          return (
            <div className="desktop-submenu-root" key={entry.id}>
              <button
                ref={submenuTriggerRef}
                className="desktop-menu-item"
                type="button"
                role="menuitem"
                tabIndex={-1}
                aria-haspopup="menu"
                aria-expanded={submenuOpen}
                data-submenu-trigger="true"
                onMouseEnter={() => setSubmenuOpen(true)}
                onClick={() => setSubmenuOpen(!submenuOpen)}
              >
                <span>{entry.label}</span>
                <ChevronRight size={14} />
              </button>
              {submenuOpen ? (
                <div className="desktop-menu desktop-submenu" role="menu">
                  {entry.entries.map((item) => (
                    <DesktopMenuItem entry={item} key={item.id} />
                  ))}
                </div>
              ) : null}
            </div>
          )
        }
        return (
          <DesktopMenuItem
            entry={entry}
            key={entry.id}
            onMouseEnter={() => setSubmenuOpen(false)}
          />
        )
      })}
    </div>
  )
}

function DesktopMenuItem({
  entry,
  onMouseEnter,
}: {
  entry: MenuCommand
  onMouseEnter?: () => void
}) {
  return (
    <button
      className="desktop-menu-item"
      type="button"
      role="menuitem"
      tabIndex={-1}
      disabled={entry.disabled}
      aria-disabled={entry.disabled || undefined}
      onMouseEnter={onMouseEnter}
      onClick={() => void entry.action()}
    >
      <span>{entry.label}</span>
      {entry.accelerator ? <kbd>{entry.accelerator}</kbd> : null}
    </button>
  )
}

function WindowControls({
  state,
  runAction,
}: {
  state: DesktopShellState
  runAction: (action: DesktopAction) => Promise<void>
}) {
  return (
    <div className="window-controls" aria-label="Window controls">
      <button
        type="button"
        aria-label="Minimize"
        title="Minimize"
        onClick={() => void runAction({ type: 'minimize' })}
      >
        <Minus size={16} strokeWidth={1.8} />
      </button>
      <button
        type="button"
        aria-label={state.isMaximized ? 'Restore' : 'Maximize'}
        title={state.isMaximized ? 'Restore' : 'Maximize'}
        onClick={() => void runAction({ type: 'toggle_maximize' })}
      >
        {state.isMaximized ? (
          <Copy className="restore-window-icon" size={14} strokeWidth={1.7} />
        ) : (
          <Square size={13} strokeWidth={1.7} />
        )}
      </button>
      <button
        className="window-close-button"
        type="button"
        aria-label="Close"
        title="Close"
        onClick={() => void runAction({ type: 'close_window' })}
      >
        <X size={17} strokeWidth={1.8} />
      </button>
    </div>
  )
}

function command(
  id: string,
  label: string,
  accelerator: string | undefined,
  action: () => void | Promise<void>,
  disabled = false,
): MenuCommand {
  return { type: 'command', id, label, accelerator, action, disabled }
}

function recentCommand(
  workspace: RecentWorkspace,
  invokeAndClose: (action: DesktopAction) => void,
): MenuCommand {
  return command(`file.open-recent.${workspace.index}`, workspaceDisplayName(workspace.label), undefined, () =>
    invokeAndClose({ type: 'open_recent', index: workspace.index }),
  )
}

function separator(id: string): MenuSeparator {
  return { type: 'separator', id }
}

function enabledMenuItems(menu: HTMLElement): HTMLElement[] {
  return Array.from(
    menu.querySelectorAll<HTMLElement>(
      ':scope > .desktop-menu-item:not([aria-disabled="true"]), :scope > .desktop-submenu-root > .desktop-menu-item:not([aria-disabled="true"])',
    ),
  )
}

function isInteractiveTarget(target: EventTarget): boolean {
  return (
    target instanceof Element &&
    Boolean(target.closest('button, a, input, textarea, select, [role="menu"]'))
  )
}

function workspaceDisplayName(label: string): string {
  const trimmed = label.replace(/[\\/]+$/, '')
  return trimmed.split(/[\\/]/).pop() || label
}
