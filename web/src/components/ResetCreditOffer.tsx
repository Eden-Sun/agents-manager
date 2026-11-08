import { ConfirmDialog } from './ConfirmDialog'
import { answerResetCreditOffer, useResetCreditOffer } from '../store/resetCreditOffer'
import { useT } from '../lib/i18n'
import { fmtCreditDate } from '../lib/resetCredits'
import { creditKeyLabel } from '../store/resetCreditsNotice'

/** 額度用完、還有重置券時的一次性提問（見 store/resetCreditOffer.ts）。掛在 App 根層，不擋其他操作。 */
export function ResetCreditOffer() {
  const t = useT()
  const offer = useResetCreditOffer()
  if (!offer) return null
  const who = creditKeyLabel(offer.key)
  return (
    <ConfirmDialog
      open
      title={t('codex 額度用完了')}
      body={
        <>
          <p>{t('{who} 的額度用完了，停工的 codex bot 會因此卡住。', { who })}</p>
          {offer.heldBots.length > 0 ? (
            <ul>
              {offer.heldBots.map((name) => (
                <li key={name}>{name}</li>
              ))}
            </ul>
          ) : null}
          <p>
            {t('還有 {n} 張重置券', { n: offer.available })}
            {offer.expiresAt ? t('（最早 {time} 到期）', { time: fmtCreditDate(offer.expiresAt) }) : ''}
            {t('。要用一張嗎？')}
          </p>
        </>
      }
      confirmLabel={t('使用一張')}
      cancelLabel={t('不用')}
      onConfirm={() => {
        void answerResetCreditOffer(offer, true)
      }}
      onCancel={() => {
        void answerResetCreditOffer(offer, false)
      }}
    />
  )
}
