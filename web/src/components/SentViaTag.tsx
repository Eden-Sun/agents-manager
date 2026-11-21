import type { Message } from "../api/types"
import { msg, useT } from "../lib/i18n"

const SENT_VIA: Record<NonNullable<Message["sent_via"]>, { text: string; title: string }> = {
  send_now: { text: msg("插隊"), title: msg("插隊送出：打斷了當時那一輪，改送這一句") },
  supplement: { text: msg("補充"), title: msg("回合中補充：直接打進終端、併在當時那一輪，沒有另開回合") },
}

export function SentViaTag({ msg: m }: { msg: Pick<Message, "role" | "sent_via"> & { source?: Message["source"] } }) {
  const t = useT()
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
