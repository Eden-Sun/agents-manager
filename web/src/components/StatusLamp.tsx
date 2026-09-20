import type { Lamp } from '../api/types'

import { LAMP_LABEL } from './lampLabel'

export function StatusLamp({ lamp, title }: { lamp: Lamp; title?: string }) {
  return (
    <span
      className={`lamp lamp-${lamp}`}
      role="img"
      aria-label={title ?? LAMP_LABEL[lamp]}
      title={title ?? LAMP_LABEL[lamp]}
    />
  )
}
