import { useState } from 'react'
import { copyText } from '../lib/copyText'
import { useT } from '../lib/i18n'

export function PaneCopy({ paneId, detail, what = 'pane' }: { paneId: string; detail: string; what?: 'pane' | 'bot' }) {
  const t = useT()
  const [done, setDone] = useState<'ok' | 'fail' | null>(null)
  return (
    <button
      type="button"
      className={`main-status pane-toggle${done === 'ok' ? ' copied' : ''}`}
      title={`${detail}\n${what === 'bot' ? t('點一下複製 bot id') : t('點一下複製 pane id')}`}
      aria-label={what === 'bot' ? t('複製 bot id {paneId}', { paneId }) : t('複製 pane id {paneId}', { paneId })}
      onClick={() => {
        void copyText(paneId).then((ok) => {
          setDone(ok ? 'ok' : 'fail')
          setTimeout(() => setDone(null), ok ? 1200 : 2400)
        })
      }}
    >
      <span className="pane-id">{done === 'ok' ? t('已複製') : done === 'fail' ? t('複製失敗') : paneId}</span>
    </button>
  )
}
