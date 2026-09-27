import type { Message } from '../api/types'

/** 回合中送出的方式（使用者 2026-09-28：「插隊或補充都要寫在右邊的對話窗，因為也是一種發出的訊息」）。 */
const SENT_VIA: Record<NonNullable<Message['sent_via']>, { text: string; title: string }> = {
  send_now: { text: '插隊', title: '插隊送出：打斷了當時那一輪，改送這一句' },
  supplement: { text: '補充', title: '回合中補充：直接打進終端、併在當時那一輪，沒有另開回合' },
}

/** 使用者訊息 meta 上的小標；一般送出不畫。 */
export function SentViaTag({ msg }: { msg: Pick<Message, 'role' | 'sent_via'> }) {
  const label = msg.role === 'user' && msg.sent_via ? SENT_VIA[msg.sent_via] : null
  if (!label) return null
  return (
    <span className="src-tag sent-via" title={label.title}>
      {label.text}
    </span>
  )
}
