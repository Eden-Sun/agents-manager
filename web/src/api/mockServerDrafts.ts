/**
 * mock 的 `/drafts`（輸入框草稿以 daemon 為準，`daemon/src/drafts.rs`）：照 daemon 的規則——rev 每個 key 單調遞增、
 * 空字串＝刪除（留墓碑）、內容沒變不加 rev；寫入後推 `draft_updated`（帶 `client_id`），所以 mock 也走得到「自己的回音」那條路。
 * 另開一個分頁就是另一個 mock：兩邊不互通，要看跨瀏覽器的同步用真 daemon。
 */
import { ApiError } from './types'

type Rec = Record<string, unknown>

const KEY = /^(bot|group):[A-Za-z0-9._-]+$/

export class MockServerDrafts {
  private rows = new Map<string, { text: string; rev: number; updated_at: string }>()

  handle(method: string, rawPath: string, b: Rec, emit: (type: string, data: unknown) => void): unknown {
    if (method === 'GET' && rawPath === '/drafts') {
      return { drafts: [...this.rows].filter(([, r]) => r.text).map(([key, r]) => ({ key, ...r })) }
    }
    const m = rawPath.match(/^\/drafts\/([^/]+)$/)
    if (method !== 'PUT' || !m) return undefined
    const key = decodeURIComponent(m[1])
    const text = typeof b.text === 'string' ? b.text : ''
    if (!KEY.test(key)) throw new ApiError(400, { error: 'bad_draft_key' }, 'bad_draft_key')
    const cur = this.rows.get(key)
    if (!cur && !text) return { key, rev: 0, updated_at: null }
    if (cur && cur.text === text) return { key, rev: cur.rev, updated_at: cur.updated_at }
    const row = { text, rev: (cur?.rev ?? 0) + 1, updated_at: new Date().toISOString() }
    this.rows.set(key, row)
    emit('draft_updated', { key, ...row, client_id: typeof b.client_id === 'string' ? b.client_id : '' })
    return { key, rev: row.rev, updated_at: row.updated_at }
  }

  /** 截圖／手動測：假裝別的瀏覽器寫了一份（`__amMock.remoteDraft('bot:x', '…')`）。 */
  remote(key: string, text: string, emit: (type: string, data: unknown) => void): void {
    this.handle('PUT', `/drafts/${encodeURIComponent(key)}`, { text, client_id: 'other-browser' }, emit)
  }
}
