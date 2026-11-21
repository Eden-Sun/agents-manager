import type { Lamp } from '../api/types'
import { useT } from '../lib/i18n'
import { COMPACTING_LABEL, LAMP_LABEL } from './lampLabel'

export function BotStateText({ lamp, reason, compacting = false }: { lamp: Lamp; reason: string; compacting?: boolean }) {
  const t = useT()
  const why = lamp === 'blocked' ? reason : ''
  return (
    <span className={`bot-state ${lamp}`} title={why || undefined}>
      {t.tr(compacting ? COMPACTING_LABEL : LAMP_LABEL[lamp])}
      {why ? <span className="bot-state-reason" title={why}>{why}</span> : null}
    </span>
  )
}
