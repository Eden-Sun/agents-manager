import { useState } from 'react'
import { copyText } from '../lib/copyText'
import { useT } from '../lib/i18n'

/**
 * 標題列的 pane id（2026-09-30 使用者）：只畫 pane id，點一下就複製；以前點開的識別列（agent／session／workspace／run id）
 * 實際上用不到，拿掉了。herdr agent 名等完整識別放 tooltip（`herdrIdentity` 的 title）。
 */
export function PaneCopy({ paneId, detail }: { paneId: string; detail: string }) {
  const t = useT()
  const [done, setDone] = useState<'ok' | 'fail' | null>(null)
  return (
    <button
      type="button"
      className={`main-status pane-toggle${done === 'ok' ? ' copied' : ''}`}
      title={`${detail}\n${t('點一下複製 pane id')}`}
      aria-label={t('複製 pane id {paneId}', { paneId })}
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
