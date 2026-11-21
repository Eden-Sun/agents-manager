
import { ApiError } from '../api/types'

export interface DraftWire {
  key: string
  text: string
  rev: number
}

export interface DraftEvent extends DraftWire {
  client_id: string
}

export interface DraftSyncDeps {
  clientId: string
  put: (key: string, text: string, clientId: string) => Promise<{ rev: number }>
  fetchAll: () => Promise<DraftWire[]>
  read: (key: string) => string
  write: (key: string, text: string) => void
  focused: (key: string) => boolean
  keys: () => string[]
  debounceMs?: number
  retryMs?: number
  onRejected?: (key: string, error: unknown) => void
  onRejectedCleared?: (key: string) => void
}

export const DRAFT_DEBOUNCE_MS = 400
export const DRAFT_RETRY_MS = 3000

type Timer = ReturnType<typeof setTimeout>

export class DraftSync {
  private readonly rev = new Map<string, number>()
  private readonly dirty = new Set<string>()
  private readonly localEpoch = new Map<string, number>()
  private readonly acceptedEpoch = new Map<string, number>()
  private readonly forgetEpoch = new Map<string, number>()
  private readonly timers = new Map<string, Timer>()
  private readonly inflight = new Set<string>()
  private readonly rejected = new Set<string>()
  private readonly shown = new Set<string>()
  private touched: Set<string> | null = null
  private loading: Promise<void> | null = null
  private readonly deps: DraftSyncDeps
  private readonly debounceMs: number
  private readonly retryMs: number

  constructor(deps: DraftSyncDeps) {
    this.deps = deps
    this.debounceMs = deps.debounceMs ?? DRAFT_DEBOUNCE_MS
    this.retryMs = deps.retryMs ?? DRAFT_RETRY_MS
  }

  get clientId(): string {
    return this.deps.clientId
  }

  localChange(key: string, text: string): void {
    this.localEpoch.set(key, (this.localEpoch.get(key) ?? 0) + 1)
    this.rejected.delete(key)
    this.dirty.add(key)
    this.touched?.add(key)
    this.schedule(key, text === '' ? 0 : this.debounceMs)
  }

  isDirty(key: string): boolean {
    return this.dirty.has(key)
  }

  knownRev(key: string): number {
    return this.rev.get(key) ?? 0
  }

  remote(ev: DraftEvent): void {
    this.touched?.add(ev.key)
    this.applyServer(ev.key, ev.text, ev.rev, ev.client_id === this.deps.clientId)
  }

  load(): Promise<void> {
    if (this.loading) return this.loading
    this.touched = new Set()
    this.loading = (async () => {
      try {
        const list = await this.deps.fetchAll()
        const touched = this.touched ?? new Set<string>()
        const seen = new Set<string>()
        for (const d of list) {
          seen.add(d.key)
          this.applyServer(d.key, d.text, d.rev, false)
        }
        for (const key of this.deps.keys()) {
          if (seen.has(key) || touched.has(key) || this.dirty.has(key) || this.inflight.has(key) || this.rejected.has(key)) continue
          this.deps.write(key, '')
        }
      } finally {
        this.touched = null
        this.loading = null
      }
      this.flushAll()
    })()
    return this.loading
  }

  flushAll(): void {
    for (const key of [...this.dirty]) this.schedule(key, 0)
  }

  forget(key: string): void {
    this.cancel(key)
    this.forgetEpoch.set(key, (this.forgetEpoch.get(key) ?? 0) + 1)
    this.dirty.delete(key)
    this.rejected.delete(key)
    this.localEpoch.delete(key)
    this.acceptedEpoch.delete(key)
    this.settle(key)
  }

  dispose(): void {
    for (const t of this.timers.values()) clearTimeout(t)
    this.timers.clear()
  }

  private applyServer(key: string, text: string, rev: number, mine: boolean): void {
    if (rev <= this.knownRev(key)) return
    this.rev.set(key, rev)
    if (mine) return
    if (this.dirty.has(key) && this.deps.focused(key)) return
    this.acceptedEpoch.set(key, this.localEpoch.get(key) ?? 0)
    this.cancel(key)
    this.dirty.delete(key)
    this.rejected.delete(key)
    this.settle(key)
    if (this.deps.read(key) !== text) this.deps.write(key, text)
  }

  private settle(key: string): void {
    if (this.shown.delete(key)) this.deps.onRejectedCleared?.(key)
  }

  private cancel(key: string): void {
    const t = this.timers.get(key)
    if (t !== undefined) clearTimeout(t)
    this.timers.delete(key)
  }

  private schedule(key: string, ms: number): void {
    this.cancel(key)
    this.timers.set(
      key,
      setTimeout(() => {
        this.timers.delete(key)
        void this.flush(key)
      }, ms),
    )
  }

  private async flush(key: string): Promise<void> {
    if (!this.dirty.has(key)) return
    if (this.inflight.has(key)) return
    const text = this.deps.read(key)
    const forgetEpoch = this.forgetEpoch.get(key) ?? 0
    this.inflight.add(key)
    const knownAtSend = this.knownRev(key)
    try {
      const { rev } = await this.deps.put(key, text, this.deps.clientId)
      const observedRev = this.knownRev(key)
      this.rev.set(key, Math.max(observedRev, rev))
      this.touched?.add(key)
      this.settle(key)
      if ((this.forgetEpoch.get(key) ?? 0) !== forgetEpoch && (this.localEpoch.get(key) ?? 0) === 0) return
      if (this.deps.read(key) === text) {
        if (observedRev > rev && observedRev > knownAtSend && this.dirty.has(key)) this.schedule(key, text === '' ? 0 : this.debounceMs)
        else this.dirty.delete(key)
      } else if ((this.localEpoch.get(key) ?? 0) > (this.acceptedEpoch.get(key) ?? 0)) {
        this.dirty.add(key)
        this.schedule(key, this.deps.read(key) === '' ? 0 : this.debounceMs)
      } else if (observedRev <= rev) {
        this.deps.write(key, text)
        this.dirty.delete(key)
      } else {
        this.dirty.delete(key)
      }
    } catch (e) {
      if (e instanceof ApiError && (e.status === 400 || e.status === 404 || e.status === 413)) {
        if (this.deps.read(key) !== text) {
          this.schedule(key, this.deps.read(key) === '' ? 0 : this.debounceMs)
          return
        }
        this.dirty.delete(key)
        this.rejected.add(key)
        this.shown.add(key)
        this.deps.onRejected?.(key, e)
      } else {
        this.schedule(key, this.retryMs)
      }
    } finally {
      this.inflight.delete(key)
    }
  }
}
