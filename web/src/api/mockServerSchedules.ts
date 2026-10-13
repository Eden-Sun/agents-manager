/**
 * mock 的專案排程（`daemon/src/schedules/api.rs` 的行為，VITE_MOCK=1 用）：
 * CRUD、同 project＋client_request_id 的冪等（同內容回原筆、不同內容 409）、手動執行（排進 mock 佇列，幾秒後變 done）、
 * 停用／刪除時撤回未送出的排隊。不模擬真的時間觸發（那是 daemon 的 tick）。寫入後推 `schedules_changed`（只帶 id）。
 */
import { ApiError, type ApiErrorBody } from './types'

function fail(status: number, error: string, message: string): never {
  throw new ApiError(status, { error, message } as ApiErrorBody, message)
}

type Rec = Record<string, unknown>

interface MockSchedule {
  id: string
  project_id: string
  bot_id: string
  name: string
  prompt: string
  mode: string
  tz: string
  spec: Rec
  next_due: string | null
  enabled: boolean
  generation: number
  notify: string
  create_crid: string | null
  create_fp: string | null
  created_at: string
  updated_at: string
  deleted_at: string | null
}

interface MockOcc {
  id: string
  schedule_id: string
  source: 'scheduled' | 'manual'
  scheduled_for: string
  generation: number
  bot_id: string
  prompt: string
  client_request_id: string | null
  status: 'pending' | 'queued' | 'done' | 'failed' | 'cancelled' | 'unknown'
  waiting_reason: string | null
  turn_status: string | null
  turn_delivery: string | null
  merged_count: number
  error: string | null
  created_at: string
  updated_at: string
}

const now = () => new Date().toISOString()
let seq = 0
const newId = (p: string) => `${p}${Date.now().toString(36)}${(seq++).toString(36)}`

export class MockServerSchedules {
  private schedules = new Map<string, MockSchedule>()
  private occs = new Map<string, MockOcc>()
  /** 手動執行的 key → 實際的 occurrence（與 daemon 的 schedule_manual_requests 同義：重放回同一筆）。 */
  private manualAlias = new Map<string, string>()
  /** 手動執行送進 fetch 的次數（測試用，看有沒有多跑）。 */
  manualRunCalls = 0

  /** `botOk(bot_id, project_id)`：同專案的一般 Bot（mock.ts 用它的 bots 表判斷）。 */
  private readonly botOk: (botId: string, projectId: string) => boolean

  constructor(botOk: (botId: string, projectId: string) => boolean) {
    this.botOk = botOk
  }

  private view(s: MockSchedule): Rec {
    const { create_crid: _c, create_fp: _f, deleted_at: _d, ...rest } = s
    return { ...rest, open: this.openOf(s.id) }
  }

  private openOf(scheduleId: string): Rec | null {
    for (const o of this.occs.values()) if (o.schedule_id === scheduleId && (o.status === 'pending' || o.status === 'queued')) return this.occView(o)
    return null
  }

  private occView(o: MockOcc): Rec {
    return { ...o }
  }

  private live(id: string): MockSchedule {
    const s = this.schedules.get(id)
    if (!s || s.deleted_at) fail(404, 'not_found', 'schedule')
    return s
  }

  private stopOpen(scheduleId: string, why: string, emit: Emit) {
    for (const o of this.occs.values()) {
      if (o.schedule_id === scheduleId && (o.status === 'pending' || o.status === 'queued')) {
        o.status = 'cancelled'
        o.error = why
        o.updated_at = now()
      }
    }
    emit('schedules_changed', { project_id: this.schedules.get(scheduleId)?.project_id, id: scheduleId })
  }

  handle(method: string, rawPath: string, b: Rec, emit: Emit): unknown {
    const pm = rawPath.match(/^\/projects\/([^/]+)\/schedules$/)
    if (pm) {
      const projectId = decodeURIComponent(pm[1])
      if (method === 'GET') {
        return { schedules: [...this.schedules.values()].filter((s) => s.project_id === projectId && !s.deleted_at).map((s) => this.view(s)) }
      }
      if (method === 'POST') return this.create(projectId, b, emit)
    }
    const sm = rawPath.match(/^\/schedules\/([^/]+)(\/run|\/history)?$/)
    if (!sm) return undefined
    const id = decodeURIComponent(sm[1])
    const sub = sm[2]
    if (!sub && method === 'GET') {
      // 詳情照 daemon 的 get_one：帶 recent（最近 10 筆）。
      const recent = [...this.occs.values()].filter((o) => o.schedule_id === id).sort((x, y) => (x.created_at < y.created_at ? 1 : -1)).slice(0, 10)
      return { ...this.view(this.live(id)), recent: recent.map((o) => this.occView(o)) }
    }
    if (!sub && method === 'PATCH') return this.patch(id, b, emit)
    if (!sub && method === 'DELETE') {
      const s = this.live(id)
      this.stopOpen(id, '排程已刪除', emit)
      s.deleted_at = now()
      s.enabled = false
      s.next_due = null
      s.updated_at = now()
      emit('schedules_changed', { project_id: s.project_id, id })
      return { ok: true, id }
    }
    if (sub === '/run' && method === 'POST') return this.run(id, b, emit)
    if (sub === '/history' && method === 'GET') {
      const rows = [...this.occs.values()].filter((o) => o.schedule_id === id).sort((x, y) => (x.created_at < y.created_at ? 1 : -1))
      return { occurrences: rows.map((o) => this.occView(o)), limit: 20, offset: 0 }
    }
    return undefined
  }

  private create(projectId: string, b: Rec, emit: Emit): Rec {
    const crid = String(b.client_request_id ?? '')
    if (!/^[A-Za-z0-9._:-]{1,128}$/.test(crid)) fail(400, 'bad', 'client_request_id 要是 1 到 128 個字元')
    const fp = JSON.stringify({ name: String(b.name ?? '').trim(), prompt: b.prompt, bot_id: b.bot_id, tz: b.tz, spec: b.spec, notify: b.notify ?? 'failure' })
    const prev = [...this.schedules.values()].find((s) => s.project_id === projectId && s.create_crid === crid)
    if (prev) {
      if (prev.create_fp !== fp) fail(409, 'client_request_id_reused', '同一個 client_request_id 已用在不同內容的排程上')
      return this.view(prev)
    }
    if (!this.botOk(String(b.bot_id), projectId)) fail(400, 'bad', '這顆 Bot 不能接收排程（需要是同專案的一般 Bot，不能是 child、分享或受限）')
    const spec = (b.spec ?? {}) as Rec
    const stamp = now()
    const s: MockSchedule = {
      id: newId('sch_'),
      project_id: projectId,
      bot_id: String(b.bot_id),
      name: String(b.name ?? '').trim(),
      prompt: String(b.prompt ?? ''),
      mode: String(spec.mode ?? 'daily'),
      tz: String(b.tz ?? 'UTC'),
      spec,
      next_due: new Date(Date.now() + 3600_000).toISOString(),
      enabled: true,
      generation: 1,
      notify: String(b.notify ?? 'failure'),
      create_crid: crid,
      create_fp: fp,
      created_at: stamp,
      updated_at: stamp,
      deleted_at: null,
    }
    this.schedules.set(s.id, s)
    emit('schedules_changed', { project_id: projectId, id: s.id })
    return this.view(s)
  }

  private patch(id: string, b: Rec, emit: Emit): Rec {
    const s = this.live(id)
    const enabled = b.enabled === undefined ? s.enabled : Boolean(b.enabled)
    const contentChanged = ['name', 'prompt', 'tz', 'notify', 'spec', 'bot_id'].some((k) => b[k] !== undefined)
    if ((contentChanged || (s.enabled && !enabled)) && this.openOf(id)) this.stopOpen(id, '排程已停用或已修改', emit)
    if (b.name !== undefined) s.name = String(b.name).trim()
    if (b.prompt !== undefined) s.prompt = String(b.prompt)
    if (b.tz !== undefined) s.tz = String(b.tz)
    if (b.notify !== undefined) s.notify = String(b.notify)
    if (b.bot_id !== undefined) s.bot_id = String(b.bot_id)
    if (b.spec !== undefined) {
      s.spec = b.spec as Rec
      s.mode = String((b.spec as Rec).mode)
    }
    if (contentChanged) s.generation += 1
    if (enabled && !s.enabled) s.next_due = new Date(Date.now() + 3600_000).toISOString()
    s.enabled = enabled
    s.updated_at = now()
    emit('schedules_changed', { project_id: s.project_id, id })
    return this.view(s)
  }

  private run(id: string, b: Rec, emit: Emit): Rec {
    const s = this.live(id)
    if (!s.enabled) fail(409, 'schedule_disabled', '排程已停用，先啟用再手動執行')
    this.manualRunCalls += 1
    const crid = typeof b.client_request_id === 'string' ? b.client_request_id : null
    const aliasKey = crid ? `${id}:${crid}` : null
    const known = aliasKey ? this.manualAlias.get(aliasKey) : undefined
    const open = this.openOf(id)
    const occ = (known ? this.occs.get(known) : undefined) ?? (open ? this.occs.get(open.id as string) : undefined) ?? this.insertManual(s, crid)
    // 不論合併進哪一筆，都記下這個 key → 實際那一筆（重試要回同一筆）。
    if (aliasKey) this.manualAlias.set(aliasKey, occ.id)
    if (occ.status === 'pending') {
      occ.status = 'queued'
      occ.turn_status = 'queued'
      occ.turn_delivery = 'pending'
      // mock 的「送出」：幾秒後變 done，讓 parent 能看見 queued → done 的轉移。
      setTimeout(() => {
        if (occ.status !== 'queued') return
        occ.status = 'done'
        occ.turn_status = 'completed'
        occ.turn_delivery = 'ok'
        occ.updated_at = now()
        emit('schedules_changed', { project_id: s.project_id, id })
        emit('schedule_notice', { project_id: s.project_id, schedule_id: id, occurrence_id: occ.id, status: 'done' })
      }, 4000)
    }
    emit('schedules_changed', { project_id: s.project_id, id })
    return { occurrence: this.occView(occ) }
  }

  private insertManual(s: MockSchedule, crid: string | null): MockOcc {
    const stamp = now()
    const o: MockOcc = {
      id: newId('occ_'),
      schedule_id: s.id,
      source: 'manual',
      scheduled_for: stamp,
      generation: s.generation,
      bot_id: s.bot_id,
      prompt: s.prompt,
      client_request_id: crid,
      status: 'pending',
      waiting_reason: null,
      turn_status: null,
      turn_delivery: null,
      merged_count: 0,
      error: null,
      created_at: stamp,
      updated_at: stamp,
    }
    this.occs.set(o.id, o)
    return o
  }
}

type Emit = (type: string, data: unknown) => void
