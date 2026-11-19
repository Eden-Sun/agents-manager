import { useState } from 'react'
import { ConfirmDialog } from './ConfirmDialog'
import { answerResetCreditOffer, snoozeResetCreditOffer, useResetCreditOffer, type ResetCreditOffer as Offer } from '../store/resetCreditOffer'
import { useT } from '../lib/i18n'
import { fmtCreditDate } from '../lib/resetCredits'
import { creditKeyLabel } from '../store/resetCreditsNotice'

/** 「不再提醒 ▾」：展開後選 1 小時／1 天／3 天，暫停這個額度的提示（只控制提醒，不兌換）。 */
function SnoozeMenu({ offer }: { offer: Offer }) {
  const t = useT()
  const [open, setOpen] = useState(false)
  // 時長選項（小時）：daemon 只收 1／24／72。
  const choices: { hours: 1 | 24 | 72; label: string }[] = [
    { hours: 1, label: t('1 小時') },
    { hours: 24, label: t('1 天') },
    { hours: 72, label: t('3 天') },
  ]
  return (
    <div className="reset-snooze">
      <button type="button" className="btn" aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        {t('不再提醒')} ▾
      </button>
      {open ? (
        <ul role="menu" className="reset-snooze-menu">
          {choices.map((c) => (
            <li key={c.hours}>
              <button
                type="button"
                role="menuitem"
                className="btn"
                onClick={() => {
                  void snoozeResetCreditOffer(offer, c.hours)
                }}
              >
                {c.label}
              </button>
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  )
}

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
          <SnoozeMenu offer={offer} />
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
