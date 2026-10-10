/**
 * 輸入框草稿與 daemon 同步（使用者 2026-10-01）：以 daemon 為準，各瀏覽器共用，不再存 localStorage。
 *
 * 這個類別只管「什麼時候送、收到別人的要不要套」，不碰 store／DOM／網路——都從 `deps` 進來，測試才能用假的。規則：
 * - 本機打字：debounce 之後 `PUT`；清空（送出）不等 debounce，馬上送，別的瀏覽器才不會留著一份會被再送一次的字。
 *   同一個 key 同時只有一個 PUT 在飛，後面的等前一個回來，daemon 看到的順序就是本機的順序（最後寫入者贏）。
 * - 收到別的 client 的 `draft_updated`：輸入框**有焦點而且本機有還沒同步的修改**就不蓋（等本機那次 PUT 送出去）；
 *   其餘都套用。自己的回音（同 `client_id`）只記 rev，不重套。
 * - `rev` 是 daemon 每個 key 單調遞增的版本：小於等於已知的 rev 一律當舊事件丟掉（WS 重播、GET 與事件交錯都靠它）。
 * - PUT 失敗（斷線）：維持 dirty、稍後重試；重連時 `load()` 重拉全部、再把還 dirty 的送出去。
 * - PUT 被 daemon 拒絕（400／413，例如超過 256 KB）：重試也不會成功，不再排重送、記進 `rejected`（草稿只留在本機），
 *   `load()` 不把它當「daemon 沒有」清掉；本機再改字、或別台寫了新版本才解除。
 */

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
  /** 目前本機輸入框的字。 */
  read: (key: string) => string
  /** 把別人的字套到本機（不可再觸發 `localChange`）。 */
  write: (key: string, text: string) => void
  /** 那個輸入框現在是不是游標所在、而且視窗在前景。 */
  focused: (key: string) => boolean
  /** 目前本機有字的所有 key（`load()` 用來找出 daemon 已經刪掉的）。 */
  keys: () => string[]
  debounceMs?: number
  retryMs?: number
  /** PUT 被 daemon 拒絕、重試無意義（400／413）。呼叫端自己去重提示。 */
  onRejected?: (key: string, error: unknown) => void
}

export const DRAFT_DEBOUNCE_MS = 400
export const DRAFT_RETRY_MS = 3000

type Timer = ReturnType<typeof setTimeout>

export class DraftSync {
  private readonly rev = new Map<string, number>()
  private readonly dirty = new Set<string>()
  private readonly timers = new Map<string, Timer>()
  private readonly inflight = new Set<string>()
  /** daemon 拒收（太大…）的 key：草稿只在本機，不重送、`load()` 也不清。 */
  private readonly rejected = new Set<string>()
  /** `load()` 在飛的時候本機碰過的 key：GET 的回應比這些寫入舊，不能拿來判「daemon 沒有它＝被刪了」。 */
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

  /** 本機改了這個輸入框（打字、倒回放回原文、退回排隊那則…）。 */
  localChange(key: string, text: string): void {
    this.rejected.delete(key)
    this.dirty.add(key)
    this.touched?.add(key)
    this.schedule(key, text === '' ? 0 : this.debounceMs)
  }

  /** 這個 key 本機還有沒送進 daemon 的修改。 */
  isDirty(key: string): boolean {
    return this.dirty.has(key)
  }

  knownRev(key: string): number {
    return this.rev.get(key) ?? 0
  }

  /** WS `draft_updated`。 */
  remote(ev: DraftEvent): void {
    // `load()` 在飛時進來的事件比那份快照新：不能再拿快照判它「daemon 沒有＝被刪了」。
    this.touched?.add(ev.key)
    this.applyServer(ev.key, ev.text, ev.rev, ev.client_id === this.deps.clientId)
  }

  /** 開頁與每次 WS 重連：拉全部、套用、再把還沒送出的送出去。同時只跑一個。 */
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
        // daemon 沒有的＝已刪（清空送出、bot 被刪）：本機沒有待送的修改才跟著清。
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

  /** 把所有還 dirty 的馬上送（重連、回到前景）。 */
  flushAll(): void {
    for (const key of [...this.dirty]) this.schedule(key, 0)
  }

  /** bot／專案被刪、草稿被清掉：不用再送。 */
  forget(key: string): void {
    this.cancel(key)
    this.dirty.delete(key)
    this.rejected.delete(key)
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
    this.cancel(key)
    this.dirty.delete(key)
    this.rejected.delete(key)
    if (this.deps.read(key) !== text) this.deps.write(key, text)
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
    // 前一個 PUT 還在飛：它回來之後會看到本機又變了、自己再排一次。
    if (this.inflight.has(key)) return
    const text = this.deps.read(key)
    this.inflight.add(key)
    try {
      const { rev } = await this.deps.put(key, text, this.deps.clientId)
      this.rev.set(key, Math.max(this.knownRev(key), rev))
      this.touched?.add(key)
      if (this.deps.read(key) === text) this.dirty.delete(key)
      else this.schedule(key, this.deps.read(key) === '' ? 0 : this.debounceMs)
    } catch (e) {
      if (e instanceof ApiError && (e.status === 400 || e.status === 413)) {
        // 重送一模一樣的字只會一直被拒：停下來，等本機改字再試。
        this.dirty.delete(key)
        this.rejected.add(key)
        this.deps.onRejected?.(key, e)
      } else {
        this.schedule(key, this.retryMs)
      }
    } finally {
      this.inflight.delete(key)
    }
  }
}
