import { useState } from 'react'
import { useT } from '../lib/i18n'

/**
 * 「待驗收」（answered）項目的兩顆按鈕：完成（→ done）、還沒好（→ following，可選填一句原因寫進 `wait_for`）。
 * 由使用者（User principal）操作；管理者交件時填 `deliverable`，這裡顯示在按鈕上方。
 */
export function AwaitingActions({ busy, onDone, onNotYet }: { busy: boolean; onDone: () => void; onNotYet: (reason: string) => void }) {
  const t = useT()
  const [notYet, setNotYet] = useState(false)
  const [reason, setReason] = useState('')
  return (
    <div className="ledger-awaiting">
      <div className="ledger-awaiting-btns">
        <button type="button" className="ledger-awaiting-done" disabled={busy} onClick={onDone}>
          {t('完成')}
        </button>
        <button type="button" className="ledger-awaiting-notyet" disabled={busy} aria-expanded={notYet} onClick={() => setNotYet((v) => !v)}>
          {t('還沒好')}
        </button>
      </div>
      {notYet ? (
        <div className="ledger-awaiting-reason">
          <input value={reason} onChange={(e) => setReason(e.target.value)} placeholder={t('一句原因（可不填）')} aria-label={t('還沒好的原因')} />
          <button type="button" disabled={busy} onClick={() => onNotYet(reason.trim())}>
            {t('送出')}
          </button>
        </div>
      ) : null}
    </div>
  )
}
