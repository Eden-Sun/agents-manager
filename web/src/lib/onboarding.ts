/**
 * 首次啟動的「環境準備」清單（issue #1931）：全新使用者一個 CLI 都沒裝、沒登入時，主畫面列出 herdr 與四種 CLI 的現況，
 * 每列旁邊給「安裝」「登入」；至少一種就緒後引導新增 Project → 新增 Bot。這裡全是純函式，顯示在 `components/OnboardingChecklist.tsx`。
 *
 * 只看**本機**（daemon 所在、使用者第一次打開的那台）。`logged_in === null` 是「問不到」，不是「未登入」：
 * 已安裝但問不到的 kind 算可用（不拿它擋住使用者、也不把它說成未登入）。
 */
import type { BotKind, IdentityStatusMap, ToolMap } from '../api/types'
import { msg } from './i18n'
import { BOT_KINDS } from '../api/types'

export type OnbKindState = 'missing' | 'out' | 'ok' | 'unknown'

export interface OnbKindRow {
  kind: BotKind
  state: OnbKindState
  version: string | null
  /** 已登入時顯示的帳號（身分狀態有 account 才有）。 */
  account: string | null
}

export interface OnbInput {
  tools: ToolMap
  identities: IdentityStatusMap
  /** 本機 daemon 與 herdr 連著。 */
  herdrConnected: boolean
  projectCount: number
}

export interface OnbModel {
  herdrConnected: boolean
  rows: OnbKindRow[]
  /** 至少一種 CLI 已裝好而且（已登入或問不到）。 */
  anyReady: boolean
  /** 主畫面要不要顯示：沒有 Project、沒有可用的 kind、或 herdr 連不上。 */
  needed: boolean
  /** 目前該做哪一步（給清單最下面的引導）。 */
  next: 'herdr' | 'cli' | 'project' | 'bot' | 'done'
  /** 還沒就緒的 kind 數（環境設定分頁的徽章）。 */
  pending: number
}

export function kindRow(kind: BotKind, tools: ToolMap, identities: IdentityStatusMap): OnbKindRow {
  const t = tools[kind]
  if (!t?.installed) return { kind, state: 'missing', version: null, account: null }
  const idLoggedIn = Object.values(identities).find((i) => i.kind === kind && i.logged_in === true)
  if (t.logged_in === true || idLoggedIn) return { kind, state: 'ok', version: t.version ?? null, account: idLoggedIn?.account ?? null }
  if (t.logged_in === false) return { kind, state: 'out', version: t.version ?? null, account: null }
  return { kind, state: 'unknown', version: t.version ?? null, account: null }
}

export function onboardingModel(i: OnbInput): OnbModel {
  const rows = BOT_KINDS.map((k) => kindRow(k, i.tools, i.identities))
  const anyReady = rows.some((r) => r.state === 'ok' || r.state === 'unknown')
  const needed = !i.herdrConnected || !anyReady || i.projectCount === 0
  const next = !i.herdrConnected ? 'herdr' : !anyReady ? 'cli' : i.projectCount === 0 ? 'project' : 'bot'
  return {
    herdrConnected: i.herdrConnected,
    rows,
    anyReady,
    needed,
    next: needed ? next : 'done',
    pending: rows.filter((r) => r.state === 'missing' || r.state === 'out').length + (i.herdrConnected ? 0 : 1),
  }
}

/** 標籤表用 `msg()` 標記，顯示處 `tr(KIND_STATE_LABEL[state])`（見 docs/FRONTEND.md〈多語系〉）。 */
export const KIND_STATE_LABEL: Record<OnbKindState, string> = {
  missing: msg('未安裝'),
  out: msg('已安裝・未登入'),
  ok: msg('已登入'),
  unknown: msg('已安裝・登入狀態問不到'),
}

/** 讓 Sidebar 打開「新增 Project」視窗的事件（Sidebar 的 modal state 是區域的，清單不 import 它以免循環）。 */
export const ADD_PROJECT_EVENT = 'am:add-project'
