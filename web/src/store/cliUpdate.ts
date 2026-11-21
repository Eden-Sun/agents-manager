import { arr, isRec, num, pick, str } from '../api/normalize'
import type { RestartBatch, RestartPlan } from '../api/types'
import { toRestartSkips } from '../api'
import { msg, t, tr } from '../lib/i18n'

type Rec = Record<string, unknown>

export interface CliUpdate {
  id: string
  host: string
  kind: string
  phase: 'starting' | 'checking' | 'installing' | 'verifying' | 'restarting'
  from: string | null
  to: string | null
}

export const CLI_UPDATE_PHASE_LABEL: Record<CliUpdate['phase'], string> = {
  starting: msg('準備安裝'),
  checking: msg('讀目前版本'),
  installing: msg('安裝中'),
  verifying: msg('確認版本'),
  restarting: msg('開始重啟'),
}

const PHASES = new Set<CliUpdate['phase']>(['checking', 'installing', 'verifying', 'restarting'])

export function cliUpdateProgress(cur: CliUpdate | null, data: Rec): CliUpdate | null {
  const id = str(pick(data, 'update_id'))
  const phase = str(pick(data, 'phase')) as CliUpdate['phase']
  if (!id || !PHASES.has(phase)) return cur
  if (cur && cur.id !== id && cur.phase !== 'starting') return cur
  return {
    id,
    host: str(pick(data, 'host'), cur?.host ?? 'local'),
    kind: str(pick(data, 'kind'), cur?.kind ?? 'codex'),
    phase,
    from: str(pick(data, 'from')) || cur?.from || null,
    to: str(pick(data, 'to')) || cur?.to || null,
  }
}

export function cliUpdateProgressMany(cur: CliUpdate[], data: Rec): CliUpdate[] {
  const next = cliUpdateProgress(null, data)
  if (!next) return cur
  const index = cur.findIndex((item) => item.id === next.id || (item.phase === 'starting' && item.host === next.host && item.kind === next.kind))
  if (index < 0) return [...cur, next]
  if (cur[index].id === next.id && cur[index].phase !== 'starting' && cur[index].phase === next.phase && cur[index].from === next.from && cur[index].to === next.to) return cur
  const out = [...cur]
  out[index] = next
  return out
}

export interface CliUpdateResult {
  id: string
  ok: boolean
  message: string
  batch: RestartBatch | null
  joinBatchId: string | null
}

const REASON: Record<string, string> = {
  install_failed: msg('安裝失敗'),
  version_unchanged: msg('裝完版本沒變'),
  target_not_reached: msg('裝完還沒到確認的版本'),
  superseded: msg('途中主機重連或改指到另一台'),
  already_running: msg('那台已經有另一個安裝在跑'),
  interrupted: msg('daemon 在安裝途中重啟過'),
}

function failureReason(kind: string, reason: string): string {
  if (reason === 'version_unreadable') return t('讀不到目前的 {kind} 版本，沒有安裝', { kind })
  if (reason === 'verify_failed') return t('裝完讀不到 {kind} 版本', { kind })
  return REASON[reason] ? tr(REASON[reason]) : reason || t('失敗')
}

function toPlan(v: unknown): RestartPlan | null {
  if (!isRec(v)) return null
  return {
    batch_id: str(pick(v, 'batch_id')),
    total: num(pick(v, 'total'), 0),
    planned: arr(pick(v, 'planned')).flatMap((x) => (isRec(x) ? [{ bot_id: str(pick(x, 'bot_id')), name: str(pick(x, 'name')) }] : [])),
    skipped: toRestartSkips(pick(v, 'skipped')),
    already_running: pick(v, 'already_running') === true,
  }
}

export function batchFromPlan(plan: RestartPlan): RestartBatch {
  return {
    id: plan.batch_id,
    total: plan.total,
    done: 0,
    current: null,
    ok: [],
    failed: [],
    skipped: plan.skipped,
    finished: plan.total === 0,
  }
}

export function cliUpdateDone(data: Rec): CliUpdateResult {
  const id = str(pick(data, 'update_id'))
  const host = str(pick(data, 'host'), 'local')
  const kind = str(pick(data, 'kind'), 'codex')
  const from = str(pick(data, 'from'))
  const to = str(pick(data, 'to'))
  if (pick(data, 'ok') !== true) {
    const reason = str(pick(data, 'reason'))
    const error = str(pick(data, 'error'))
    const head = t('{host} 的 {kind} 安裝沒有完成（{reason}），沒有重啟任何 Bot', { host, kind, reason: failureReason(kind, reason) })
    return { id, ok: false, message: error ? t('{head}：{error}', { head, error }) : head, batch: null, joinBatchId: null }
  }
  const plan = toPlan(pick(data, 'restart'))
  const up = from && to && from !== to ? `${from} → ${to}` : to || t('新版')
  const verb = pick(data, 'already_installed') === true ? t('本來就是') : t('已升到')
  if (kind === 'claude' && pick(data, 'restart_required') === true) {
    return { id, ok: true, message: t('{host} 的 claude {verb} {up}；請按 header 的更新按鈕，在確認框按「重啟套用」', { host, verb, up }), batch: null, joinBatchId: null }
  }
  if (!plan) {
    const why = str(pick(data, 'restart_error'))
    const whyText = why ? t('：{why}', { why }) : ''
    return { id, ok: true, message: t('{host} 的 {kind} {verb} {up}，但重啟沒排起來{why}；請再按一次 header 的更新按鈕', { host, kind, verb, up, why: whyText }), batch: null, joinBatchId: null }
  }
  const status = str(pick(data, 'restart_status'))
  if (status === 'deferred') {
    return { id, ok: true, message: t('{host} 的 {kind} {verb} {up}；已經有一批重啟在跑，codex 排在那批後面，跑完會自動接著重啟', { host, kind, verb, up }), batch: null, joinBatchId: null }
  }
  if (status === 'already_covered') {
    return { id, ok: true, message: t('{host} 的 {kind} {verb} {up}；正在跑的那批重啟已經排著這台的 codex', { host, kind, verb, up }), batch: null, joinBatchId: plan.batch_id }
  }
  if (plan.already_running) {
    return { id, ok: true, message: t('{host} 的 {kind} {verb} {up}；已經有一批重啟在跑，跑完後再按 header 的更新按鈕重啟 codex', { host, kind, verb, up }), batch: null, joinBatchId: plan.batch_id }
  }
  const tail = plan.total > 0 ? t('，重啟 {n} 顆閒置的 {kind}', { n: plan.total, kind }) : t('；沒有閒置的 {kind} 可以重啟（在忙的等閒下來，再按 header 的更新按鈕）', { kind })
  return { id, ok: true, message: t('{host} 的 {kind} {verb} {up}{tail}', { host, kind, verb, up, tail }), batch: batchFromPlan(plan), joinBatchId: null }
}

export function cliUpdateDoneMany(cur: CliUpdate[], data: Rec): CliUpdate[] {
  const { id } = cliUpdateDone(data)
  return id ? cur.filter((item) => item.id !== id) : cur
}

export function reconcileCliUpdate(cur: CliUpdate | null, running: { update_id: string; host: string }[] | undefined): CliUpdate | null {
  if (!cur || running === undefined || cur.phase === 'starting') return cur
  return running.some((r) => r.update_id === cur.id) ? cur : null
}

export function reconcileCliUpdates(cur: CliUpdate[], running: { update_id: string; host: string }[] | undefined): CliUpdate[] {
  if (running === undefined) return cur
  const active = new Set(running.map((row) => row.update_id))
  const next = cur.filter((item) => item.phase === 'starting' || active.has(item.id))
  return next.length === cur.length && next.every((item, i) => item === cur[i]) ? cur : next
}
