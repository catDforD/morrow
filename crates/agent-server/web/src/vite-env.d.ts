/// <reference types="vite/client" />

interface Window {
  readonly __MORROW_DESKTOP__?: Readonly<{
    platform: 'windows' | 'macos' | 'linux'
  }>
}
