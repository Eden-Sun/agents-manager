/**
 * header 一鍵升級 codex（SPEC §6.9，API `POST /api/hosts/{name}/cli-update`）的進度怎麼累加。
 * 拆出來是因為事件處理在 socket 裡，store 動作測不到它。
 *
 * daemon 只回 `update_id`，進度走 WS：`cli_update_progress`（`checking`→`installing`→`verifying`→`restarting`）、
 * `cli_update_done`（`ok`＋版本；失敗帶 `reason`／`error`；成功帶 `restart`＝一鍵重啟的計畫，之後照一鍵重啟的事件走）。
 */
import { arr, isRec, num, pick, str } from '../api/normalize'
import type { RestartBatch, RestartPlan } from '../api/types'
import { toRestartSkips } from '../api'

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
  starting: '準備安裝',
  checking: '讀目前版本',
  installing: '安裝中',
  verifying: '確認版本',
  restarting: '開始重啟',
}

const PHASES = new Set<CliUpdate['phase']>(['checking', 'installing', 'verifying', 'restarting'])

/** 一則 `cli_update_progress`。手上沒有（別的分頁按的）也收：header 要看得到那台正在裝。 */
export function cliUpdateProgress(cur: CliUpdate | null, data: Rec): CliUpdate | null {
  const id = str(pick(data, 'update_id'))
  const phase = str(pick(data, 'phase')) as CliUpdate['phase']
  if (!id || !PHASES.has(phase)) return cur
  // 手上那一次已經換成另一個 id（後按的）：舊的事件不蓋掉它。
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

export interface CliUpdateResult {
  ok: boolean
  /** 給 notify 的一句話。 */
  message: string
  /** 成功而且真的開了一批：把它當成一鍵重啟的進度接手。 */
  batch: RestartBatch | null
  /** 已經有一批在跑，codex 這次沒排進去（要等那批跑完再按一次）。 */
  joinBatchId: string | null
}

const REASON: Record<string, string> = {
  version_unreadable: '讀不到目前的 codex 版本，沒有安裝',
  install_failed: '安裝失敗',
  verify_failed: '裝完讀不到 codex 版本',
  version_unchanged: '裝完版本沒變',
  target_not_reached: '裝完還沒到確認的版本',
  superseded: '途中主機重連或改指到另一台',
  already_running: '那台已經有另一個安裝在跑',
  interrupted: 'daemon 在安裝途中重啟過',
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

/** 一鍵重啟的計畫 → header 的進度（跟 `restartIdleBots` 同一份形狀）。 */
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

/** 一則 `cli_update_done`。失敗一律沒有重啟任何 bot（daemon 的保證），訊息照 `reason` 講。 */
export function cliUpdateDone(data: Rec): CliUpdateResult {
  const host = str(pick(data, 'host'), 'local')
  const from = str(pick(data, 'from'))
  const to = str(pick(data, 'to'))
  if (pick(data, 'ok') !== true) {
    const reason = str(pick(data, 'reason'))
    const error = str(pick(data, 'error'))
    const head = `${host} 的 codex 升級沒有完成（${REASON[reason] ?? reason ?? '失敗'}），沒有重啟任何 Bot`
    return { ok: false, message: error ? `${head}：${error}` : head, batch: null, joinBatchId: null }
  }
  const plan = toPlan(pick(data, 'restart'))
  const up = from && to && from !== to ? `${from} → ${to}` : to || '新版'
  // 磁碟上本來就是那一版（#569）：沒有跑安裝指令，只改通知、開重啟。
  const verb = pick(data, 'already_installed') === true ? '本來就是' : '已升到'
  if (!plan) {
    const why = str(pick(data, 'restart_error'))
    return { ok: true, message: `${host} 的 codex ${verb} ${up}，但重啟沒排起來${why ? `：${why}` : ''}；再按一次 ⌃⌃ 重啟`, batch: null, joinBatchId: null }
  }
  if (plan.already_running) {
    return { ok: true, message: `${host} 的 codex ${verb} ${up}；已經有一批重啟在跑，那批跑完再按一次 ⌃⌃ 重啟 codex`, batch: null, joinBatchId: plan.batch_id }
  }
  const tail = plan.total > 0 ? `，重啟 ${plan.total} 顆閒置的 codex` : '；沒有閒置的 codex 可以重啟（在忙的之後再按 ⌃⌃）'
  return { ok: true, message: `${host} 的 codex ${verb} ${up}${tail}`, batch: batchFromPlan(plan), joinBatchId: null }
}

/**
 * 快照的 `cli_updates` 跟手上這一份對帳（同 #492 的 `restart_batch`）：`undefined`＝舊 daemon 沒這欄＝不知道，不動；
 * daemon 說那台沒在裝了，手上那份就是 `cli_update_done` 收不到留下的，清掉（chip 變回可以按）。
 */
export function reconcileCliUpdate(cur: CliUpdate | null, running: { update_id: string; host: string }[] | undefined): CliUpdate | null {
  if (!cur || running === undefined || cur.phase === 'starting') return cur
  return running.some((r) => r.update_id === cur.id) ? cur : null
}
