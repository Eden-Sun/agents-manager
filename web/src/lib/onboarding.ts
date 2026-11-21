import type { BotKind, IdentityStatusMap, ToolMap } from '../api/types'
import { msg } from './i18n'
import { BOT_KINDS } from '../api/types'

export type OnbKindState = 'missing' | 'out' | 'ok' | 'unknown'

export interface OnbKindRow {
  kind: BotKind
  state: OnbKindState
  version: string | null
  account: string | null
}

export interface OnbInput {
  tools: ToolMap
  identities: IdentityStatusMap
  herdrConnected: boolean
  projectCount: number
}

export interface OnbModel {
  herdrConnected: boolean
  rows: OnbKindRow[]
  anyReady: boolean
  needed: boolean
  next: 'herdr' | 'cli' | 'project' | 'bot' | 'done'
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

export const KIND_STATE_LABEL: Record<OnbKindState, string> = {
  missing: msg('未安裝'),
  out: msg('已安裝・未登入'),
  ok: msg('已登入'),
  unknown: msg('已安裝・登入狀態問不到'),
}

export const ADD_PROJECT_EVENT = 'am:add-project'
