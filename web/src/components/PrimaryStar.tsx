/**
 * 「主要 bot」開關：使用者自己的分類，釘起來後固定排在 `UnreadChip` 最前面。
 * 存 daemon 而非瀏覽器，手機與電腦追同一組；不影響啟動參數，`needs_restart` 恆為 false。
 */
import { useStore } from '../store/store'

export function PrimaryStar({ botId }: { botId: string }) {
  const on = useStore((s) => s.bots.find((b) => b.id === botId)?.primary ?? false)
  const busy = useStore((s) => Boolean(s.busy[`patch:${botId}`]))
  const patchBot = useStore((s) => s.patchBot)
  return (
    <button
      type="button"
      className={`icon-btn primary-star icon-tip${on ? ' on' : ''}`}
      aria-pressed={on}
      aria-label={on ? '取消主要' : '設為主要'}
      data-tip={on ? '取消「主要執行的 bot」' : '設為「主要執行的 bot」（會固定排在上面那一列）'}
      disabled={busy}
      onClick={() => void patchBot(botId, { primary: !on })}
    >
      {on ? '★' : '☆'}
    </button>
  )
}
