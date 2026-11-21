import { useStore } from '../store/store'
import { t, useT } from '../lib/i18n'

export function PrimaryStar({ botId }: { botId: string }) {
  useT()
  const on = useStore((s) => s.bots.find((b) => b.id === botId)?.primary ?? false)
  const busy = useStore((s) => Boolean(s.busy[`patch:${botId}`]))
  const patchBot = useStore((s) => s.patchBot)
  return (
    <button
      type="button"
      className={`icon-btn primary-star icon-tip${on ? ' on' : ''}`}
      aria-pressed={on}
      aria-label={on ? t('取消主要') : t('設為主要')}
      data-tip={on ? t('取消「主要執行的 bot」') : t('設為「主要執行的 bot」（會固定排在上面那一列）')}
      disabled={busy}
      onClick={() => void patchBot(botId, { primary: !on })}
    >
      {on ? '★' : '☆'}
    </button>
  )
}
