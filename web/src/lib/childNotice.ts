import type { Message } from '../api/types'

export const CHILD_ALERT_MARK = '[daemon 自動通知，不是 bot_request]'
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

export function isChildNoticeRequestId(id: string | null | undefined): boolean {
  return !!id && (id.startsWith('child-done:') || id.startsWith('child-blocked:'))
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

export function isChildNoticeTurn(messages: readonly Message[], turnId: string | null, requestId?: string | null): boolean {
  if (turnId && messages.some((m) => m.turn_id === turnId && m.role === 'user' && !childNotice(m))) return false
  return isChildNoticeRequestId(requestId) || (turnId !== null && childNoticeTurns(messages).has(turnId))
}

export type ChildNoticeRow = { kind: 'message'; message: Message } | {
  kind: 'notices'; id: string; messages: Message[]; notices: ChildNotice[]; turnIds: Set<string>
}

/** Keep order intact: any ordinary message breaks a consecutive run of notice turns. */
export function groupChildNotices(messages: readonly Message[]): ChildNoticeRow[] {
  const turns = childNoticeTurns(messages)
  const byTurn = new Map(messages.flatMap((m) => {
    const notice = childNotice(m)
    return notice && m.turn_id ? [[m.turn_id, notice] as const] : []
  }))
  const rows: ChildNoticeRow[] = []
  for (const msg of messages) {
    if (!msg.turn_id || !turns.has(msg.turn_id) || (msg.role !== 'user' && msg.role !== 'assistant')) {
      rows.push({ kind: 'message', message: msg })
      continue
    }
    let row = rows.at(-1)
    if (row?.kind !== 'notices') {
      row = { kind: 'notices', id: msg.id, messages: [], notices: [], turnIds: new Set() }
      rows.push(row)
    }
    row.messages.push(msg)
    const notice = byTurn.get(msg.turn_id)
    if (notice && !row.turnIds.has(msg.turn_id)) row.notices.push(notice)
    row.turnIds.add(msg.turn_id)
  }
  return rows
}

export function childNoticeLabel(notices: readonly ChildNotice[]): string {
  const parts = (['done', 'blocked'] as const).flatMap((status) => {
    const names = [...new Set(notices.filter((n) => n.status === status).map((n) => n.name))]
    return names.length ? [`child ${names.join('、')} ${status === 'done' ? '回報完成' : '在等人回答'}`] : []
  })
  return `↳ ${parts.join('；')}${notices.length > 1 ? `（${notices.length}）` : ''}`
}
