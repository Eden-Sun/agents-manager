/**
 * mock 的 herdr 一鍵更新（SPEC §6.9，API `POST /api/hosts/{name}/herdr-update`）：只演示進度、結果與上游快照，不碰任何 herdr。
 * `__amMock.herdrUpdate()` 推一筆 `kind:"herdr"` 的上游快照（local 與 m4p 都落後；m4p 是遠端，按鈕那台只會是 local），header 就會出現徽章；
 * `__amMock.herdrUpdate({ failOne: true })` 演一顆沒接回，`{ reason: 'busy_timeout' }` 演整次沒做、`{ reason: 'restart_failed' }` 演換回舊版（多一步 `rolling_back`，bot 照樣接回）。
 */
import { ApiError } from './types'

type Rec = Record<string, unknown>

export interface MockHerdrCtx {
  emit: (type: string, data: unknown) => void
  /** 那台在跑的 bot（有 active run）。 */
  liveBots: (host: string) => { bot_id: string; name: string; parent_bot_id: string | null; run_id: string }[]
  setUpstream: (item: Rec) => void
  hostKnown: (host: string) => boolean
  /** 演示用：把本機還沒在跑的 bot 都開起來，確認框才有「接回／子 agent」名單可看。 */
  startLocal: () => void
}

export interface MockHerdrOpts {
  target?: string
  installed?: string
  failOne?: boolean
  reason?: string
  /** 停在某個階段不往下走（截進度圖用）。 */
  hold?: string
}

const STEPS: [number, string][] = [
  [300, 'downloading'],
  [1500, 'waiting_idle'],
  [4500, 'stopping'],
  [5200, 'restarting'],
  [6400, 'resuming'],
]

const DETAIL: Record<string, string> = {
  busy_timeout: '等了 1800s 仍有 bot 在忙：am-claude (working)；什麼都沒動',
  restart_failed: '新版 2 分鐘內沒以 0.9.3 回來，已換回 herdr.bak-0.9.1 再重啟',
  download_failed: '下載 herdr-linux-x86_64 失敗：HTTP 404',
}

export class MockHerdrUpdate {
  private running: { update_id: string; host: string; target_version: string; phase: string; started_at: string } | null = null
  private opts: MockHerdrOpts = {}
  private target = '0.9.3'
  private installed = '0.9.1'
  private seq = 0
  private ctx: MockHerdrCtx
  constructor(ctx: MockHerdrCtx) {
    this.ctx = ctx
  }

  /** `GET /api/state` 的 `herdr_updates`。 */
  rows() {
    return this.running ? [{ ...this.running }] : []
  }

  private item(localInstalled: string): Rec {
    const hosts = [
      { host: 'local', installed_version: localInstalled, error: null, behind: localInstalled !== this.target },
      { host: 'm4p', installed_version: this.installed, error: null, behind: true },
    ]
    const behind = hosts.filter((h) => h.behind)
    return {
      kind: 'herdr', latest_version: this.target, target_version: this.target, source_url: 'https://github.com/herdrdev/herdr/releases',
      checked_at: new Date().toISOString(), error: null, has_update: behind.length > 0, notified_version: this.target, notify: null,
      hosts,
      // 同 daemon `upstream_update::notice_text` 的 herdr 那句。
      text: behind.length
        ? `herdr 上游有新版 ${this.target}（${behind.map((h) => `${h.host} 是 ${h.installed_version}`).join('、')}）：更新會重啟 herdr server，所有 bot 中斷約 1 分鐘後自動接回`
        : null,
    }
  }

  seed(opts: MockHerdrOpts = {}) {
    this.opts = opts
    this.target = opts.target ?? '0.9.3'
    this.installed = opts.installed ?? '0.9.1'
    this.ctx.startLocal()
    this.ctx.setUpstream(this.item(this.installed))
  }

  start(host: string, b: Rec): Rec {
    if (!this.ctx.hostKnown(host)) throw new ApiError(404, { error: 'not_found', what: `host ${host}` }, 'not found')
    // 形狀與訊息照 daemon `herdr_upgrade.rs`（API.md §12.7b）。
    if (host !== 'local') throw new ApiError(409, { error: 'conflict', reason: 'unsupported_host', host, message: 'herdr 一鍵更新目前只支援本機' }, 'conflict')
    const target = String(b.target_version ?? '')
    if (target !== this.target) {
      throw new ApiError(409, {
        error: 'conflict', reason: 'stale_target', host, target_version: target, current_target: this.target,
        message: `${host} 的 herdr 現在要升的是 ${this.target}，不是確認框寫的 ${target}；重新開確認框再按一次`,
      }, 'conflict')
    }
    if (this.running) {
      throw new ApiError(409, {
        error: 'conflict', reason: 'herdr_update_in_progress', update_id: this.running.update_id, host: this.running.host,
        message: `herdr 已經在更新（${this.running.phase}），這一下沒有再開一次`,
      }, 'conflict')
    }
    const update_id = `herdrup-${++this.seq}`
    const live = this.ctx.liveBots(host)
    const will_resume = live.filter((x) => !x.parent_bot_id).map((x) => ({ bot_id: x.bot_id, name: x.name }))
    const children_lost = live.filter((x) => x.parent_bot_id).map((x) => ({ bot_id: x.bot_id, name: x.name, parent_bot_id: x.parent_bot_id }))
    this.running = { update_id, host, target_version: this.target, phase: 'downloading', started_at: new Date().toISOString() }
    const base = { update_id, host, target_version: this.target }
    const steps: [number, string][] = this.opts.reason === 'busy_timeout'
      ? STEPS.slice(0, 2)
      : this.opts.reason === 'restart_failed'
        ? [...STEPS.slice(0, 4), [5800, 'rolling_back'], [6400, 'resuming']]
        : STEPS
    for (const [at, phase] of steps) {
      setTimeout(() => {
        if (this.running?.update_id !== update_id) return
        this.running.phase = phase
        this.ctx.emit('herdr_update_progress', { ...base, phase })
      }, at)
      if (phase === this.opts.hold) return { ...base, started: true, will_resume, children_lost }
    }
    setTimeout(() => {
      this.running = null
      const reason = this.opts.reason ?? null
      const ok = !reason
      const fail = this.opts.failOne && will_resume.length > 0 ? will_resume[will_resume.length - 1] : null
      const resumed = reason && reason !== 'restart_failed' ? [] : live.filter((x) => !x.parent_bot_id && x.bot_id !== fail?.bot_id).map((x) => ({ bot_id: x.bot_id, name: x.name, run_id: x.run_id }))
      if (ok) this.ctx.setUpstream(this.item(this.target))
      // daemon 的 `to` 一律是目標版（失敗也是）。
      this.ctx.emit('herdr_update_done', {
        update_id, host, ok, from: this.installed, to: this.target,
        ...(reason ? { reason, detail: DETAIL[reason] ?? reason } : {}),
        resumed,
        failed: fail ? [{ ...fail, error: 'resume 失敗：找不到 session，要重開' }] : [],
        children_lost: reason && reason !== 'restart_failed' ? [] : children_lost,
      })
    }, steps[steps.length - 1][0] + 1200)
    return { ...base, started: true, will_resume, children_lost }
  }
}
