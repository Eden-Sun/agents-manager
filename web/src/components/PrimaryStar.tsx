/**
 * 標題列上的「把這顆設成主要執行的 bot」開關。
 *
 * 「主要」是使用者自己的分類，不是 daemon 算得出來的東西：一堆 bot 裡真正在推進工作的
 * 通常只有兩三顆，其餘是備援、實驗、或某次任務留下來的。釘起來之後它們會固定排在標題列
 * 下面那一列的最前面（`UnreadChip`），不管有沒有未讀、在不在跑。
 *
 * 存在 daemon（`PATCH /api/bots/:id {primary}`）而不是瀏覽器：使用者在手機與電腦上追的
 * 是同一組 bot。這個欄位不影響啟動參數，所以 `needs_restart` 永遠是 false。
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
