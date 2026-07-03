export async function fetchJson<T>(url: string, options?: RequestInit): Promise<T> {
  const response = await fetch(url, options)
  if (!response.ok) {
    const body = await response.json().catch(() => ({}))
    const message =
      typeof body.error === 'string'
        ? body.error
        : `${response.status} ${response.statusText}`
    throw new Error(message)
  }
  return response.json() as Promise<T>
}

export function sessionSocketUrl(name: string): string {
  const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${protocol}//${location.host}/api/sessions/${encodeURIComponent(name)}/ws`
}
