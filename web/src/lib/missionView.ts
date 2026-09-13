/**
 * 任務卡要顯示的東西，全部從「任務欄位＋事件串」推出來（`docs/goals/agm-missions.md` §5）。
 *
 * daemon 刻意**不存**細分狀態（API.md：「規劃／執行／審查／驗證的細分之後由該任務的
 * assignments 推導，不另存一份狀態」），所以五段進度、誰是執行者／reviewer／驗證者、撞限換了
 * 幾次手，都是在這裡算的。
 *
 * 事件 payload 的欄位名在 P1b（assignment 加 `mission_id/role/turn_error`）落地前還沒定死，
 * 所以每一個讀取都是**寬鬆**的：讀得到就顯示，讀不到就留白，絕不讓卡片因此壞掉或說謊。
 */
import type { Mission, MissionAssignment, MissionEvent, MissionRole } from '../api/types'

/** 進度條上的五段＋終點。`paused` / `cancelled` 不是其中一格，是疊在上面的狀態。 */
export type MissionPhase = 'planning' | 'executing' | 'reviewing' | 'verifying' | 'delivering' | 'done'

export const MISSION_PHASES: readonly MissionPhase[] = [
  'planning',
  'executing',
  'reviewing',
  'verifying',
  'delivering',
  'done',
]

export const PHASE_LABEL: Record<MissionPhase, string> = {
  planning: '規劃',
  executing: '執行',
  reviewing: '審查',
  verifying: '驗證',
  delivering: '交付',
  done: '完成',
}

/** 卡片上會出現的狀態：五段＋停下來的三種。 */
export type MissionViewPhase = MissionPhase | 'paused' | 'cancelled' | 'waiting_quota' | 'awaiting_agm'

/** 狀態 chip 上的字。五段用 `PHASE_LABEL`，停下來的那幾種另外講。 */
export function phaseLabel(p: MissionViewPhase): string {
  if (p === 'paused') return '等你回答'
  if (p === 'cancelled') return '已取消'
  if (p === 'waiting_quota') return '等額度'
  if (p === 'awaiting_agm') return '等 AGM'
  return PHASE_LABEL[p]
}

export const ROLE_LABEL: Record<MissionRole, string> = {
  executor: '執行者',
  reviewer: 'reviewer',
  verifier: '驗證者',
}

/** 某個角色現在由誰擔任。欄位讀不到就是 `null`，不要猜。 */
export interface MissionActor {
  role: MissionRole
  /** bot 名稱（讀不到就退回 bot id / relay_from）。 */
  bot: string | null
  identity: string | null
  model: string | null
  at: string
}

/** 撞限換手：cc2 用完換 cc1 的那一次。 */
export interface MissionHandoff {
  from: string | null
  to: string | null
  /** `limit_hit` / `5h` / 事件自己寫的原因。 */
  reason: string
  at: string
}

export interface MissionDelivered {
  mode: string
  sha: string | null
  branch: string | null
  url: string | null
}

/** 停下來問人時，卡片上要顯示什麼。 */
export interface MissionAsk {
  /** `max_rounds` / `no_fable_for_verifier` / `push_main_failed` / `pr_failed` / 自訂。 */
  reason: string
  detail: string | null
  /** AGM 在群組問的那句話（最後一則 `paused` 事件的文字）。 */
  question: string | null
  /** `no_fable_for_verifier` 時各身分的 Fable 重置時間。 */
  resets: { identity: string; resets_at: string }[]
}

export interface MissionView {
  phase: MissionViewPhase
  /** 進度走到第幾格（`MISSION_PHASES` 的索引）；停下來時停在停下來之前那一格。 */
  step: number
  actors: MissionActor[]
  handoffs: MissionHandoff[]
  /** §10：找不到第二個身分當 reviewer，改走「執行者自審＋驗證者把關」。 */
  soloReview: boolean
  rounds: { used: number; max: number }
  ask: MissionAsk | null
  verified: MissionEvent | null
  delivered: MissionDelivered | null
  /** 最後一則有內容的回報，摺疊時當一行摘要用。 */
  latest: MissionEvent | null
}

function rec(v: unknown): Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : {}
}

function text(v: unknown): string | null {
  return typeof v === 'string' && v.length > 0 ? v : null
}

function roleOf(e: MissionEvent): MissionRole | null {
  const r = text(rec(e.payload).role)
  return r === 'executor' || r === 'reviewer' || r === 'verifier' ? r : null
}

/** `paused` 事件帶的 `resets:[{identity, resets_at}]`（驗證者找不到 Fable 額度時）。 */
function resetsOf(payload: Record<string, unknown>): { identity: string; resets_at: string }[] {
  const raw = payload.resets
  if (!Array.isArray(raw)) return []
  const out: { identity: string; resets_at: string }[] = []
  for (const r of raw) {
    const o = rec(r)
    const identity = text(o.identity)
    if (identity) out.push({ identity, resets_at: text(o.resets_at) ?? '' })
  }
  return out
}

/**
 * 進度走到哪裡。
 *
 * 只看「有沒有發生過」而不是「現在在做什麼」——事件是往前推進的流水帳，最遠走到哪一段就是
 * 哪一段。`verified` 之後就等交付，所以算進 `delivering`。
 */
function progressOf(m: Mission, events: MissionEvent[]): MissionPhase {
  if (m.completed_at) return 'done'
  let far: MissionPhase = 'planning'
  const reach = (p: MissionPhase) => {
    if (MISSION_PHASES.indexOf(p) > MISSION_PHASES.indexOf(far)) far = p
  }
  for (const e of events) {
    if (e.kind === 'delivered' || e.kind === 'completed') reach('delivering')
    if (e.kind === 'verified') reach('delivering')
    const role = roleOf(e)
    if (role === 'verifier') reach('verifying')
    if (role === 'reviewer') reach('reviewing')
    if (role === 'executor') reach('executing')
  }
  return far
}

/**
 * 把任務欄位、事件串與交辦串算成任務卡要的東西。
 *
 * 有 P1b 的 `assignments[]` 就以它為準——`role` 與 `turn_error` 是 daemon 寫的，比從事件
 * payload 猜可靠；身分與模型還是只有事件帶得出來，兩邊併起來用。
 */
export function missionView(m: Mission, events: MissionEvent[], assignments: MissionAssignment[] = []): MissionView {
  const byRole = new Map<MissionRole, MissionActor>()
  const handoffs: MissionHandoff[] = []
  let soloReview = false
  let verified: MissionEvent | null = null
  let delivered: MissionDelivered | null = null
  let latest: MissionEvent | null = null
  let lastPaused: MissionEvent | null = null

  for (const e of events) {
    const p = rec(e.payload)
    const role = roleOf(e)
    if (role) {
      // 同一個角色換過人就顯示最新的那一位（撞限換手之後卡片要指向現在在跑的那顆）。
      byRole.set(role, {
        role,
        bot: text(p.bot_name) ?? text(p.bot) ?? text(e.relay_from),
        identity: text(p.identity),
        model: text(p.model),
        at: e.created_at,
      })
    }
    if (p.handoff === true || text(p.reason) === 'limit_hit') {
      handoffs.push({
        from: text(p.from) ?? text(p.from_identity),
        to: text(p.to) ?? text(p.to_identity),
        reason: text(p.reason) ?? 'limit_hit',
        at: e.created_at,
      })
    }
    if (p.no_independent_reviewer === true || text(p.decision) === 'no_independent_reviewer') soloReview = true
    if (e.kind === 'verified') verified = e
    if (e.kind === 'delivered') {
      delivered = {
        mode: text(p.mode) ?? m.delivery_mode,
        sha: text(p.sha),
        branch: text(p.branch),
        url: text(p.url),
      }
    }
    if (e.kind === 'paused') lastPaused = e
    if (e.text) latest = e
  }

  // 交辦是 daemon 寫的：角色一定對，bot 也一定是現在那一顆。身分／模型補事件裡的。
  for (const a of assignments) {
    if (!a.role) continue
    const from = byRole.get(a.role)
    byRole.set(a.role, {
      role: a.role,
      bot: a.target_bot_id ?? from?.bot ?? null,
      identity: from?.identity ?? null,
      model: from?.model ?? null,
      at: a.created_at || (from?.at ?? ''),
    })
    // 撞限換手在交辦上是「follow-up ＋ 上一件帶著 turn_error」，事件沒帶也認得出來。
    if (a.follow_up_of && !handoffs.some((h) => h.at === a.created_at)) {
      const parent = assignments.find((x) => x.id === a.follow_up_of)
      const err = parent?.turn_error ?? null
      if (err || parent?.turn_status === 'identity_switch') {
        handoffs.push({ from: null, to: null, reason: err ?? 'identity_switch', at: a.created_at })
      }
    }
  }

  const progress = progressOf(m, events)
  // daemon 給了 `phase` 就用它（它看得到交辦，比事件準）；沒給才用事件推的。
  const server = m.phase
  const phase: MissionViewPhase = m.cancelled_at
    ? 'cancelled'
    : m.completed_at
      ? 'done'
      : m.paused_reason
        ? 'paused'
        : server === 'waiting_quota' || server === 'awaiting_agm'
          ? server
          : server === 'executing' || server === 'reviewing' || server === 'verifying' || server === 'planning'
            ? server
            : progress

  return {
    phase,
    // 進度格：daemon 的 phase 落在五段裡就用它，否則用事件推的（等額度／等 AGM 也停在原地）。
    step: Math.max(MISSION_PHASES.indexOf(progress), server ? MISSION_PHASES.indexOf(server as MissionPhase) : -1),
    actors: [...byRole.values()].sort(
      (a, b) => ['executor', 'reviewer', 'verifier'].indexOf(a.role) - ['executor', 'reviewer', 'verifier'].indexOf(b.role),
    ),
    handoffs,
    soloReview,
    rounds: { used: m.rounds_used, max: m.max_rounds },
    ask: m.paused_reason
      ? {
          reason: m.paused_reason,
          detail: m.paused_detail,
          question: lastPaused?.text ?? null,
          resets: resetsOf(rec(lastPaused?.payload)),
        }
      : null,
    verified,
    delivered,
    latest,
  }
}

/** 停下來問人的原因，講成人話。認不得的機器碼原樣顯示，不要吞掉。 */
export function pausedLabel(reason: string): string {
  switch (reason) {
    case 'max_rounds':
      return '來回次數用完了'
    case 'no_fable_for_verifier':
      return '沒有可用的 Fable 額度可以當驗證者'
    case 'push_main_failed':
      return '推 main 失敗'
    case 'pr_failed':
      return '開 PR 失敗'
    default:
      return reason
  }
}

/** 交付方式講成人話。 */
export function deliveryLabel(mode: string): string {
  return mode === 'push_main' ? '直接推 main' : '開 PR'
}
