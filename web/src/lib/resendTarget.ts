import type { Message } from '../api/types'

export interface ResendTarget {
  text: string
  attachments: string[]
}

/** 最後一則 user 訊息（文字＋附件 id）；沒有 user 訊息、或文字與附件都是空的回 null。 */
export function resendTarget(list: readonly Pick<Message, 'role' | 'content' | 'attachments'>[]): ResendTarget | null {
  for (let i = list.length - 1; i >= 0; i -= 1) {
    const message = list[i]
    if (message.role !== 'user') continue
    const attachments = message.attachments.map((attachment) => attachment.id)
    return message.content.trim() || attachments.length ? { text: message.content, attachments } : null
  }
  return null
}
