import { useT } from '../lib/i18n'
export function AwaitingActions({ busy, onDone, onNotSure }: { busy: boolean; onDone: () => void; onNotSure: () => void }) {
  const t = useT()
  return (
    <div className="ledger-awaiting">
      <div className="ledger-awaiting-btns">
        <button type="button" className="ledger-awaiting-done" disabled={busy} onClick={onDone}>
          {t('完成')}
        </button>
        <button type="button" className="ledger-awaiting-notsure" disabled={busy} onClick={onNotSure}>
          {t('不知道')}
        </button>
      </div>
    </div>
  )
}
