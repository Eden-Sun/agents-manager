import type { CloseNeedsConfirm } from "../api"
import { useT } from "../lib/i18n"

/** 關 pane／結束 shell 被 daemon 擋下（服務 pane、或讀不到它在跑什麼）時，第二道確認框的說明（AGM 驗收 9f05b03）。 */
export function ServicePaneNote({ needs }: { needs: CloseNeedsConfirm | null }) {
  const t = useT()
  if (needs?.unverified) {
    return <>{t("daemon 讀不到這顆 pane 現在在跑什麼，無法確認裡面沒有 dev server 之類的服務。確定要關嗎？")}</>
  }
  const ports = needs?.pane?.listen_ports.length ? needs.pane.listen_ports.join(t("、")) : null
  const fg = needs?.pane?.foreground ?? null
  if (ports && fg) {
    return <>{t.x("這顆 pane 正在 listen {ports}（{fg}）。關掉它等於把裡面的服務一起停掉。確定要關嗎？", {
      ports: <strong>{ports}</strong>,
      fg: <code>{fg}</code>,
    })}</>
  }
  if (ports) {
    return <>{t.x("這顆 pane 正在 listen {ports}。關掉它等於把裡面的服務一起停掉。確定要關嗎？", {
      ports: <strong>{ports}</strong>,
    })}</>
  }
  if (fg) {
    return <>{t.x("這顆 pane 正在 listen（{fg}）。關掉它等於把裡面的服務一起停掉。確定要關嗎？", {
      fg: <code>{fg}</code>,
    })}</>
  }
  return <>{t("這顆 pane 正在 listen。關掉它等於把裡面的服務一起停掉。確定要關嗎？")}</>
}
