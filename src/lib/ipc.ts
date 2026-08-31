import type { NativeState } from '../types'

type IpcResponse<T> = { id: string; success: boolean; data: T }

declare global {
  interface Window {
    ipc?: { postMessage: (message: string) => void }
    __RDESKTOP_IPC__?: (response: IpcResponse<unknown>) => void
    __RDESKTOP_WINDOW__?: {
      minimize: () => void
      maximize: () => void
      close: () => void
      startDrag: () => void
    }
  }
}

const pending = new Map<string, { resolve: (value: unknown) => void; reject: (reason: Error) => void }>()

function readNativeCapability() {
  if (typeof window === 'undefined' || !window.location.hash.startsWith('#ipc=')) return undefined
  const value = new URLSearchParams(window.location.hash.slice(1)).get('ipc')?.trim()
  return value && value.length <= 128 ? value : undefined
}

const nativeCapability = readNativeCapability()

if (typeof window !== 'undefined') {
  window.__RDESKTOP_IPC__ = (response) => {
    const request = pending.get(response.id)
    if (!request) return
    pending.delete(response.id)
    if (response.success) request.resolve(response.data)
    else {
      const data = response.data as { message?: unknown } | string | null | undefined
      const message = typeof data === 'string' ? data : typeof data?.message === 'string' ? data.message : 'Native request failed'
      request.reject(new Error(message))
    }
  }
}

export async function invoke<T>(cmd: string, payload: Record<string, unknown> = {}, timeoutMs = 12_000): Promise<T> {
  const id = `mailgo-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`

  if (window.ipc?.postMessage) {
    return new Promise<T>((resolve, reject) => {
      pending.set(id, { resolve: resolve as (value: unknown) => void, reject })
      const nativePayload = nativeCapability ? { ...payload, __mailgoCapability: nativeCapability } : payload
      window.ipc?.postMessage(JSON.stringify({ id, cmd, payload: nativePayload }))
      window.setTimeout(() => {
        if (!pending.has(id)) return
        pending.delete(id)
        reject(new Error(`Native request timed out: ${cmd}`))
      }, timeoutMs)
    })
  }

  if (!import.meta.env.DEV) {
    throw new Error('Native IPC is unavailable in this packaged renderer')
  }

  const response = await fetch('/__rdesktop__/agent/ipc', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id, cmd, payload }),
  })
  if (!response.ok) throw new Error(`Dev bridge request failed: ${response.status}`)
  const envelope = (await response.json()) as IpcResponse<T>
  if (!envelope.success) {
    const data = envelope.data as { message?: unknown } | string | null | undefined
    const message = typeof data === 'string' ? data : typeof data?.message === 'string' ? data.message : 'Dev bridge request failed'
    throw new Error(message)
  }
  return envelope.data
}

export async function readNativeState(): Promise<NativeState | null> {
  try {
    return await invoke<NativeState>('app.get_state')
  } catch {
    return null
  }
}
