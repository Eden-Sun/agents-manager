import { useStore } from "../store/store"
import { useT } from "../lib/i18n"
import "./blockedBadge.css"

export function BlockedBadge({ botId, onOpen }: { botId: string; onOpen: () => void }) {
  const t = useT()
  const blocked = useStore((s) => s.runs[botId]?.agent_status === "blocked")
  if (!blocked) return null
  return (
    <button
      type="button"
      className="blocked-badge"
      title={t("agent 停在一個要你回答的提示上，點一下開全畫面終端回答")}
      onClick={onOpen}
    >
      <span className="blocked-badge-dot" aria-hidden="true" />
      {t("需要回應")}
    </button>
  )
}
