import type { Message } from "../api/types"
import { msg, useT } from "../lib/i18n"

/** 回合中送出的方式（使用者 2026-09-28：「插隊或補充都要寫在右邊的對話窗，因為也是一種發出的訊息」）。 */
const SENT_VIA: Record<NonNullable<Message["sent_via"]>, { text: string; title: string }> = {
  send_now: { text: msg("插隊"), title: msg("插隊送出：打斷了當時那一輪，改送這一句") },
  supplement: { text: msg("補充"), title: msg("回合中補充：直接打進終端、併在當時那一輪，沒有另開回合") },
}

/** 使用者訊息 meta 上的小標；一般送出不畫。 */
export function SentViaTag({ msg: m }: { msg: Pick<Message, "role" | "sent_via"> & { source?: Message["source"] } }) {
  const t = useT()
  // 分享頁的 end user 送來的（SPEC「分享 bot」）：跟你自己打的分開，一眼看出是外部的人。
  if (m.role === "user" && m.source === "share") {
    return (
      <span className="src-tag share-user" title={t("從分享連結送來的：不是你，是拿到連結的外部使用者")}>
        {t("🔗 分享使用者")}
      </span>
    )
  }
  const label = m.role === "user" && m.sent_via ? SENT_VIA[m.sent_via] : null
  if (!label) return null
  return (
    <span className="src-tag sent-via" title={t.tr(label.title)}>
      {t.tr(label.text)}
    </span>
  )
}
