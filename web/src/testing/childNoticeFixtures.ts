import type { Message } from '../api/types'
import { CHILD_ALERT_MARK } from '../lib/childNotice'

export function message(id: string, turn: string | null, role: Message['role'], content = id, extra: Partial<Message> = {}): Message {
  return { id, turn_id: turn, role, content, conversation_id: 'c', bot_id: 'b', source: 'web', relay_from: null,
    incomplete: false, group_id: null, attachments: [], terminal_snapshot: null, created_at: '2026-10-15T01:02:00Z', ...extra }
}
export const notice = (id: string, name = 'sc1779', blocked = false) => message(id, id, 'user',
  `${CHILD_ALERT_MARK}子 agent ${name} ${blocked ? '的回合停在 blocked，在等人回答。' : '已完成一個回合。'}\n\n通知原文的尾巴`, { relay_from: 'child' })
