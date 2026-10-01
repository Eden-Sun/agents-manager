/**
 * herdr 一鍵更新（SPEC §6.9，完整重啟版；API `POST /api/hosts/{name}/herdr-update`）的前端狀態。
 *
 * 跟 codex／claude 的 `cliUpdate.ts` 同一套路：daemon 只回 `update_id`（加上要接回／會失去的名單），進度走 WS
 * `herdr_update_progress`（`downloading`→`waiting_idle`→`stopping`→`restarting`→`resuming`）、結果走 `herdr_update_done`，
 * 重整或漏幀時用 `GET /api/state` 的 `herdr_updates` 對帳。
 *
 * 自己一個 store、不塞進 `useStore`：同時只會有一次（daemon 409 `herdr_update_in_progress`），而且結果要留著讓人點開看
 * 「誰接回來、誰沒接回、哪些子 agent 沒了」，跟 `useStore` 的快照替換無關。
 */
import { create } from 'zustand'
import { arr, isRec, pick, str } from '../api/normalize'
import { startHerdrUpdate, herdrStartErrText, toChildrenLost, type HerdrBotRef, type HerdrChildLost } from '../api/herdrUpdate'
import type { Bot, Run } from '../api/types'
import type { UpstreamItem } from './upstreamUpdate'

type Rec = Record<string, unknown>

export type HerdrPhase = 'starting' | 'downloading' | 'waiting_idle' | 'stopping' | 'restarting' | 'resuming'

export const HERDR_PHASE_LABEL: Record<HerdrPhase, string> = {
  starting: '準備更新',
  downloading: '下載新版',
  waiting_idle: '等所有 Bot 閒下來',
  stopping: '停下 herdr',
  restarting: '重啟 herdr',
  resuming: '接回 Bot',
}

const PHASES = new Set<HerdrPhase>(['downloading', 'waiting_idle', 'stopping', 'restarting', 'resuming'])

export interface HerdrUpdateActive {
  /** `''`＝POST 還沒回來。 */
  id: string
  host: string
  target: string
  phase: HerdrPhase
  willResume: HerdrBotRef[]
  childrenLost: HerdrChildLost[]
}

export interface HerdrUpdateResult {
  id: string
  host: string
  ok: boolean
  from: string | null
  to: string | null
  reason: string | null
  error: string | null
  resumed: (HerdrBotRef & { run_id: string })[]
  failed: (HerdrBotRef & { error: string })[]
  childrenLost: HerdrChildLost[]
}

/** 一則 `herdr_update_progress`。手上沒有（別的分頁按的）也收：header 要看得到正在更新。 */
export function herdrProgress(cur: HerdrUpdateActive | null, data: Rec): HerdrUpdateActive | null {
  const id = str(pick(data, 'update_id'))
  const phase = str(pick(data, 'phase')) as HerdrPhase
  if (!id || !PHASES.has(phase)) return cur
  if (cur && cur.id && cur.id !== id) return cur
  if (cur && cur.id === id && cur.phase === phase) return cur
  return {
    id,
    host: str(pick(data, 'host'), cur?.host ?? 'local'),
    target: str(pick(data, 'target_version'), cur?.target ?? ''),
    phase,
    willResume: cur?.willResume ?? [],
    childrenLost: cur?.childrenLost ?? [],
  }
}

export function parseHerdrDone(data: Rec): HerdrUpdateResult {
  return {
    id: str(pick(data, 'update_id')),
    host: str(pick(data, 'host'), 'local'),
    ok: pick(data, 'ok') === true,
    from: str(pick(data, 'from')) || null,
    to: str(pick(data, 'to')) || null,
    reason: str(pick(data, 'reason')) || null,
    error: str(pick(data, 'error')) || null,
    resumed: arr(pick(data, 'resumed')).flatMap((x) =>
      isRec(x) && str(pick(x, 'bot_id')) ? [{ bot_id: str(pick(x, 'bot_id')), name: str(pick(x, 'name')), run_id: str(pick(x, 'run_id')) }] : [],
    ),
    failed: arr(pick(data, 'failed')).flatMap((x) =>
      isRec(x) && str(pick(x, 'bot_id')) ? [{ bot_id: str(pick(x, 'bot_id')), name: str(pick(x, 'name')), error: str(pick(x, 'error')) }] : [],
    ),
    childrenLost: toChildrenLost(pick(data, 'children_lost')),
  }
}

const REASON: Record<string, string> = {
  busy_timeout: '等了 30 分鐘還有 Bot 在忙，什麼都沒動',
  restart_failed: '新版 herdr 起不來，已換回舊版',
  download_failed: '下載新版失敗，什麼都沒動',
  verify_failed: '下載的檔案版本不對，什麼都沒動',
  interrupted: 'daemon 在更新途中重啟過',
}

export function herdrReasonText(reason: string | null): string {
  return reason ? (REASON[reason] ?? reason) : '失敗'
}

/** 給 notify 的一句話。 */
export function herdrDoneMessage(r: HerdrUpdateResult): string {
  const back = r.resumed.length || r.failed.length
    ? `接回 ${r.resumed.length} 顆${r.failed.length ? `，${r.failed.length} 顆沒接回` : ''}`
    : ''
  const kids = r.childrenLost.length ? `${r.childrenLost.length} 個子 agent 已結束，已請母 Bot 視需要重開` : ''
  const tail = [back, kids].filter(Boolean).join('；')
  if (r.ok) {
    const up = r.from && r.to && r.from !== r.to ? `${r.from} → ${r.to}` : (r.to ?? '新版')
    return `${r.host} 的 herdr 已升到 ${up}${tail ? `；${tail}` : ''}（點 header 的 ✓ 看名單）`
  }
  const head = `${r.host} 的 herdr 沒有升級（${herdrReasonText(r.reason)}）`
  return `${head}${r.error ? `：${r.error}` : ''}${tail ? `；${tail}` : ''}`
}

/**
 * 快照的 `herdr_updates` 對帳（同 `reconcileCliUpdates`）：`undefined`＝舊 daemon 沒這欄＝不知道，不動；
 * daemon 有一筆在跑就採用（重整後接得回進度）；daemon 說沒有了，手上那份就是 `herdr_update_done` 漏掉留下的，清掉。
 */
export function reconcileHerdr(cur: HerdrUpdateActive | null, rows: { update_id: string; host: string; target_version: string; phase: string }[] | undefined): HerdrUpdateActive | null {
  if (rows === undefined) return cur
  if (cur?.phase === 'starting') return cur
  const row = (cur ? rows.find((r) => r.update_id === cur.id) : undefined) ?? rows[0]
  if (!row) return null
  const phase = PHASES.has(row.phase as HerdrPhase) ? (row.phase as HerdrPhase) : (cur?.phase ?? 'downloading')
  const same = cur?.id === row.update_id
  if (same && cur.phase === phase) return cur
  return {
    id: row.update_id,
    host: row.host,
    target: row.target_version,
    phase,
    willResume: same ? cur.willResume : [],
    childrenLost: same ? cur.childrenLost : [],
  }
}

export interface HerdrPlanHost {
  host: string
  installed: string | null
  error: string | null
  behind: boolean
  /** 這台不能從這裡更新的原因；`null`＝可以。 */
  blocked: string | null
}

export interface HerdrUpdatePlan {
  target: string
  hosts: HerdrPlanHost[]
  /** 按鈕會更新的那台（落後而且支援）；`null`＝沒有能按的，原因看 `blockedWhy`。 */
  host: string | null
  from: string | null
  blockedWhy: string | null
  /** 前端估的名單（實際以 202 回的 `will_resume` 為準）：那台在跑的頂層 Bot。 */
  willResume: { botId: string; name: string }[]
  /** 那台在跑的子 agent：herdr 重啟後一律沒了，要由母 Bot 重開。 */
  childrenLost: { botId: string; name: string; parentName: string }[]
}

export function herdrHostBlocked(host: string, sharedHosts: ReadonlySet<string>): string | null {
  if (sharedHosts.has(host)) return '共用 session：別的 daemon 也在用這台的 herdr，不能從這裡重啟'
  if (host !== 'local') return '遠端主機：這版只支援更新本機的 herdr，請到那台手動更新'
  return null
}

/** header herdr 徽章的內容：上游有新版、而且至少一台落後才出現。 */
export function herdrUpdatePlan(
  item: UpstreamItem | null | undefined,
  sharedHosts: ReadonlySet<string>,
  bots: Bot[],
  runs: Record<string, Run | null>,
  hostOf: (bot: Bot) => string,
): HerdrUpdatePlan | null {
  if (!item || item.kind !== 'herdr' || !item.hasUpdate || !item.target) return null
  const hosts = item.hosts.map((h) => ({
    host: h.host,
    installed: h.installedVersion,
    error: h.error,
    behind: h.behind,
    blocked: herdrHostBlocked(h.host, sharedHosts),
  }))
  if (!hosts.some((h) => h.behind)) return null
  const chosen = hosts.find((h) => h.behind && !h.blocked) ?? null
  const blockedWhy = chosen ? null : (hosts.find((h) => h.behind)?.blocked ?? null)
  const on = chosen?.host ?? 'local'
  const live = bots.filter((b) => !b.pending && runs[b.id] && hostOf(b) === on)
  const nameOf = (id: string | null) => bots.find((b) => b.id === id)?.name ?? '（已不在）'
  return {
    target: item.target,
    hosts,
    host: chosen?.host ?? null,
    from: chosen?.installed ?? hosts.find((h) => h.host === 'local')?.installed ?? null,
    blockedWhy,
    willResume: chosen ? live.filter((b) => !b.parent_bot_id).map((b) => ({ botId: b.id, name: b.name })) : [],
    childrenLost: chosen
      ? live.filter((b) => b.parent_bot_id).map((b) => ({ botId: b.id, name: b.name, parentName: nameOf(b.parent_bot_id) }))
      : [],
  }
}

interface HerdrUpdateState {
  active: HerdrUpdateActive | null
  /** 最近一次的結果；徽章點開看名單、看完收起。 */
  result: HerdrUpdateResult | null
}

export const useHerdrUpdate = create<HerdrUpdateState>(() => ({ active: null, result: null }))

type Notify = (kind: 'info' | 'error', text: string) => void

export async function startHerdr(host: string, target: string, notify: Notify): Promise<void> {
  if (useHerdrUpdate.getState().active) return
  useHerdrUpdate.setState({ active: { id: '', host, target, phase: 'starting', willResume: [], childrenLost: [] }, result: null })
  try {
    const r = await startHerdrUpdate(host, target)
    useHerdrUpdate.setState((s) => {
      const cur = s.active
      // 進度事件可能比回應先到（已經換成真的 id 與階段），那就只補名單。
      if (cur && (cur.id === '' || cur.id === r.update_id)) {
        return { active: { ...cur, id: r.update_id, willResume: r.will_resume, childrenLost: r.children_lost } }
      }
      return {}
    })
  } catch (e) {
    useHerdrUpdate.setState((s) => (s.active?.phase === 'starting' ? { active: null } : {}))
    notify('error', `herdr 更新沒開始：${herdrStartErrText(e, host)}`)
  }
}

/** WS 幀；回要跳的通知（只有 done 會有）。 */
export function onHerdrFrame(type: string, data: Rec): { kind: 'info' | 'error'; text: string } | null {
  if (type === 'herdr_update_progress') {
    useHerdrUpdate.setState((s) => {
      const next = herdrProgress(s.active, data)
      return next !== s.active ? { active: next } : {}
    })
    return null
  }
  if (type !== 'herdr_update_done') return null
  const r = parseHerdrDone(data)
  useHerdrUpdate.setState((s) => ({
    active: s.active && s.active.id && s.active.id !== r.id ? s.active : null,
    result: r,
  }))
  return { kind: r.ok && r.failed.length === 0 ? 'info' : 'error', text: herdrDoneMessage(r) }
}

export function applyHerdrSnapshot(rows: { update_id: string; host: string; target_version: string; phase: string }[] | undefined) {
  useHerdrUpdate.setState((s) => {
    const next = reconcileHerdr(s.active, rows)
    return next !== s.active ? { active: next } : {}
  })
}

export function dismissHerdrResult() {
  useHerdrUpdate.setState({ result: null })
}


/** 手機合成那顆（`MergedUpdateChip`）選單裡 herdr 那一項的文案；`null`＝這顆不出現。 */
export function herdrMenuItem(active: HerdrUpdateActive | null, result: HerdrUpdateResult | null, plan: HerdrUpdatePlan | null): string | null {
  if (active) return `herdr 更新中：${HERDR_PHASE_LABEL[active.phase]}…`
  if (result) return result.ok ? `herdr 已升到 ${result.to ?? '新版'}（看名單）` : `herdr 沒有升級（${herdrReasonText(result.reason)}）`
  if (!plan) return null
  return plan.host ? `更新 herdr ${plan.target}（所有 Bot 中斷約 1 分鐘）` : `herdr ${plan.target}：不能從這裡更新`
}
