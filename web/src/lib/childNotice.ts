import type { Message } from '../api/types'
import { t } from './i18n'

export const CHILD_ALERT_MARK = '[daemon 自動通知，不是 bot_request]' // i18n-ignore: exact marker emitted by daemon
export interface ChildNotice { name: string; status: 'done' | 'blocked' }

/** Only daemon relays with the exact leading marker; quoted user text stays visible. */
export function childNotice(msg: Message): ChildNotice | null {
  if (msg.role !== 'user' || !msg.relay_from || msg.relay_unverified || !msg.turn_id || !msg.content.startsWith(CHILD_ALERT_MARK)) return null
  const line = msg.content.slice(CHILD_ALERT_MARK.length).split('\n', 1)[0]
  const done = /^子 agent (.+) 已完成一個回合。$/.exec(line)
  if (done) return { name: done[1], status: 'done' }
  const blocked = /^子 agent (.+) 的回合停在 blocked，在等人回答。$/.exec(line)
  return blocked ? { name: blocked[1], status: 'blocked' } : null
}

/** A user supplement in the same turn must remain visible, together with its reply. */
export function childNoticeTurns(messages: readonly Message[]): Set<string> {
  const turns = new Set<string>()
  const userTurns = new Set<string>()
  for (const msg of messages) {
    if (!msg.turn_id || msg.role !== 'user') continue
    if (childNotice(msg)) turns.add(msg.turn_id)
    else userTurns.add(msg.turn_id)
  }
  for (const id of userTurns) turns.delete(id)
  return turns
}

export type ChildNoticeRow = { kind: 'message'; message: Message } | {
  kind: 'notices'; id: string; messages: Message[]; notices: ChildNotice[]; turnIds: Set<string>
}

/**
 * Only the daemon's notice itself folds; the parent's reply to it stays a normal message, because that reply is the
 * parent reporting to the user (2026-10-16 使用者：w2ER:p6Y「回報完成」收起來後 parent 的回報看不到). Adjacent notices
 * still share one row; any other message breaks the run so order stays intact.
 */
export function groupChildNotices(messages: readonly Message[]): ChildNoticeRow[] {
  const turns = childNoticeTurns(messages)
  const rows: ChildNoticeRow[] = []
  for (const msg of messages) {
    const notice = msg.turn_id && turns.has(msg.turn_id) ? childNotice(msg) : null
    if (!notice || !msg.turn_id) {
      rows.push({ kind: 'message', message: msg })
      continue
    }
    let row = rows.at(-1)
    if (row?.kind !== 'notices') {
      row = { kind: 'notices', id: msg.id, messages: [], notices: [], turnIds: new Set() }
      rows.push(row)
    }
    row.messages.push(msg)
    if (!row.turnIds.has(msg.turn_id)) row.notices.push(notice)
    row.turnIds.add(msg.turn_id)
  }
  return rows
}

export function childNoticeLabel(notices: readonly ChildNotice[]): string {
  const parts = (['done', 'blocked'] as const).flatMap((status) => {
    const names = [...new Set(notices.filter((n) => n.status === status).map((n) => n.name))]
    return names.length
      ? [status === 'done'
        ? t('子 agent 已回報完成：{names}', { names: names.join(', ') })
        : t('子 agent 正在等人回答：{names}', { names: names.join(', ') })]
      : []
  })
  return notices.length > 1
    ? t('↳ {parts} ({n})', { parts: parts.join('; '), n: notices.length })
    : t('↳ {parts}', { parts: parts.join('; ') })
}
