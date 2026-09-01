import type { NativeState } from '../types'

type IpcResponse<T> = { id: string; success: boolean; data: T }

type PendingRequest = {
  resolve: (value: unknown) => void
  reject: (reason: Error) => void
  timeoutId: number
}

declare global {
  interface Window {
    ipc?: { postMessage: (message: string) => void }
    __RDESKTOP_IPC__?: (response: IpcResponse<unknown> | string) => void
    __RDESKTOP_WINDOW__?: {
      minimize: () => void
      maximize: () => void
      close: () => void
      startDrag: () => void
    }
  }
}

const pending = new Map<string, PendingRequest>()

function readNativeCapability() {
  if (typeof window === 'undefined' || !window.location.hash.startsWith('#ipc=')) return undefined
  const value = new URLSearchParams(window.location.hash.slice(1)).get('ipc')?.trim()
  return value && value.length <= 128 ? value : undefined
}

const nativeCapability = readNativeCapability()

if (typeof window !== 'undefined') {
  window.__RDESKTOP_IPC__ = (incoming) => {
    let response: IpcResponse<unknown>
    try {
      response = typeof incoming === 'string' ? JSON.parse(incoming) as IpcResponse<unknown> : incoming
    } catch {
      return
    }
    if (!response || typeof response.id !== 'string' || typeof response.success !== 'boolean') return
    const request = pending.get(response.id)
    if (!request) return
    pending.delete(response.id)
    window.clearTimeout(request.timeoutId)
    if (response.success) request.resolve(response.data)
    else {
      const data = response.data as { message?: unknown } | string | null | undefined
      const message = typeof data === 'string' ? data : typeof data?.message === 'string' ? data.message : 'Native request failed'
      request.reject(new Error(message))
    }
  }
}

export async function invoke<T>(cmd: string, payload: Record<string, unknown> = {}, timeoutMs = 12_000): Promise<T> {
  const requestSuffix = typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`
  const id = `mailgo-${requestSuffix}`

  if (window.ipc?.postMessage) {
    return new Promise<T>((resolve, reject) => {
      const timeoutId = window.setTimeout(() => {
        const request = pending.get(id)
        if (!request) return
        pending.delete(id)
        request.reject(new Error(`Native request timed out: ${cmd}`))
      }, timeoutMs)
      pending.set(id, { resolve: resolve as (value: unknown) => void, reject, timeoutId })
      const nativePayload = nativeCapability ? { ...payload, __mailgoCapability: nativeCapability } : payload
      try {
        window.ipc?.postMessage(JSON.stringify({ id, cmd, payload: nativePayload }))
      } catch (error) {
        window.clearTimeout(timeoutId)
        pending.delete(id)
        reject(error instanceof Error ? error : new Error(`Native request failed: ${cmd}`))
      }
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
  return invoke<NativeState>('app.get_state')
}
