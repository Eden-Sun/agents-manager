import { useT } from '../lib/i18n'
/**
 * 「待驗收」（answered）項目的兩顆按鈕：完成（→ done）、不知道（→ 進行中＋複查指示，下一輪自動提醒管理者複查）。
 * 不按＝維持待驗收、什麼都不做。
 */
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
