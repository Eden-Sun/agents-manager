import type { Lamp } from '../api/types'
import { useT } from '../lib/i18n'

import { backgroundLabel } from '../lib/backgroundJobs'
import { LAMP_LABEL } from './lampLabel'
import { CAPACITY_LABEL } from '../lib/modelCapacity'
import './backgroundJobs.css'
import './modelCapacity.css'

export function StatusLamp({ lamp, title, background = 0, kids = 0, capacity = false }: { lamp: Lamp; title?: string; background?: number;  kids?: number;  capacity?: boolean }) {
  const t = useT()
  const bg = lamp === 'idle' && background > 0 ? background : 0
  const spin = lamp === 'idle' && kids > 0
  const full = capacity && lamp === 'idle'
  const text = title ?? (full ? t.tr(CAPACITY_LABEL) : spin ? t('子 agent 還在跑（{n}）', { n: kids }) : bg > 0 ? backgroundLabel(bg) : t.tr(LAMP_LABEL[lamp]))
  return <span className={`lamp lamp-${lamp}${spin ? ' lamp-bg' : ''}${full ? ' lamp-capacity' : ''}`} role="img" aria-label={text} title={text} />
}
