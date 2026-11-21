import { useStore } from '../store/store'
import { CAPACITY_LABEL, modelAtCapacity } from '../lib/modelCapacity'
import { useT } from '../lib/i18n'
import './modelCapacity.css'

export function ModelCapacityNote({ botId }: { botId: string }) {
  const t = useT()
  const capacity = useStore((s) => modelAtCapacity(s.runs[botId]))
  if (!capacity) return null
  return (
    <div className="composer-capacity" role="status">
      <strong>{t.tr(CAPACITY_LABEL)}</strong>
      <span>{t('OpenAI 目前容量不夠，這一回合沒有跑——不是 bot 卡住，也不是額度用完。可以點標題列的模型標籤換一個模型，或稍後再送一次；不會自動重送。')}</span>
    </div>
  )
}
