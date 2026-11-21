import { ApiError } from './types'

type Rec = Record<string, unknown>

const KEY = /^((bot|group):[A-Za-z0-9._-]+|shell:[A-Za-z0-9._:@+-]+\/[A-Za-z0-9._:@+-]+)$/

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

  remote(key: string, text: string, emit: (type: string, data: unknown) => void): void {
    this.handle('PUT', `/drafts/${encodeURIComponent(key)}`, { text, client_id: 'other-browser' }, emit)
  }
}
