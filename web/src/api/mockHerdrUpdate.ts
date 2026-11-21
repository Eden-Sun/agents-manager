import { later } from './mockClock'
import { ApiError } from './types'
import { L } from './mockText'

type Rec = Record<string, unknown>

export interface MockHerdrCtx {
  emit: (type: string, data: unknown) => void
  liveBots: (host: string) => { bot_id: string; name: string; parent_bot_id: string | null; run_id: string }[]
  setUpstream: (item: Rec) => void
  hostKnown: (host: string) => boolean
  startLocal: () => void
}

export interface MockHerdrOpts {
  target?: string
  installed?: string
  failOne?: boolean
  reason?: string
  hold?: string
}

const STEPS: [number, string][] = [
  [300, 'downloading'],
  [1500, 'waiting_idle'],
  [4500, 'stopping'],
  [5200, 'restarting'],
  [6400, 'resuming'],
]

const detail = (): Record<string, string> => ({
  busy_timeout: L('等了 1800s 仍有 bot 在忙：am-claude (working)；什麼都沒動', 'Waited 1800s and a bot is still busy: am-claude (working); nothing was changed'),
  restart_failed: L('新版 2 分鐘內沒以 0.9.3 回來，已換回 herdr.bak-0.9.1 再重啟', 'The new version did not come back as 0.9.3 within 2 minutes; rolled back to herdr.bak-0.9.1 and restarted'),
  download_failed: L('下載 herdr-linux-x86_64 失敗：HTTP 404', 'Downloading herdr-linux-x86_64 failed: HTTP 404'),
})

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

  rows() {
    return this.running ? [{ ...this.running }] : []
  }

  private item(localInstalled: string): Rec {
    const hosts = [
      { host: 'local', installed_version: localInstalled, error: null, behind: localInstalled !== this.target },
      { host: 'mac_builder', installed_version: this.installed, error: null, behind: true },
    ]
    const behind = hosts.filter((h) => h.behind)
    return {
      kind: 'herdr', latest_version: this.target, target_version: this.target, source_url: 'https://github.com/herdrdev/herdr/releases',
      checked_at: new Date().toISOString(), error: null, has_update: behind.length > 0, notified_version: this.target, notify: null,
      hosts,
      text: behind.length
        ? L(
            `herdr 上游有新版 ${this.target}（${behind.map((h) => `${h.host} 是 ${h.installed_version}`).join('、')}）：更新會重啟 herdr server，所有 bot 中斷約 1 分鐘後自動接回`,
            `herdr ${this.target} is available upstream (${behind.map((h) => `${h.host} is on ${h.installed_version}`).join(', ')}): updating restarts the herdr server, and all bots disconnect for about 1 minute, then resume automatically`,
          )
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
      later(() => {
        if (this.running?.update_id !== update_id) return
        this.running.phase = phase
        this.ctx.emit('herdr_update_progress', { ...base, phase })
      }, at)
      if (phase === this.opts.hold) return { ...base, started: true, will_resume, children_lost }
    }
    later(() => {
      this.running = null
      const reason = this.opts.reason ?? null
      const ok = !reason
      const fail = this.opts.failOne && will_resume.length > 0 ? will_resume[will_resume.length - 1] : null
      const resumed = reason && reason !== 'restart_failed' ? [] : live.filter((x) => !x.parent_bot_id && x.bot_id !== fail?.bot_id).map((x) => ({ bot_id: x.bot_id, name: x.name, run_id: x.run_id }))
      if (ok) this.ctx.setUpstream(this.item(this.target))
      this.ctx.emit('herdr_update_done', {
        update_id, host, ok, from: this.installed, to: this.target,
        ...(reason ? { reason, detail: detail()[reason] ?? reason } : {}),
        resumed,
        failed: fail ? [{ ...fail, error: 'resume 失敗：找不到 session，要重開' }] : [],
        children_lost: reason && reason !== 'restart_failed' ? [] : children_lost,
      })
    }, steps[steps.length - 1][0] + 1200)
    return { ...base, started: true, will_resume, children_lost }
  }
}
