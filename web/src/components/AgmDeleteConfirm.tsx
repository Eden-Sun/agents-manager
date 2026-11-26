import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'

export function AgmDeleteConfirm() {
  const t = useT()
  const ask = useStore((s) => s.agmDeleteAsk)
  const cancel = useStore((s) => s.cancelAgmDelete)
  const removeBot = useStore((s) => s.removeBot)
  const batch = useStore((s) => s.agmDeleteBatch)
  const confirmBatch = useStore((s) => s.confirmAgmBatch)
  const cancelBatch = useStore((s) => s.cancelAgmBatch)
  return (
    <>
    <ConfirmDialog
      open={batch !== null}
      title={t('這些是 AGM 的 Bot')}
      body={
        batch ? (
          <>
            {t('其中 {n} 顆是 AGM 的 Bot：{names}。', { n: batch.length, names: batch.map((a) => a.name).join('、') })}
            <ul className="confirm-choices">
              <li>{t('刪了 AGM 可能會壞：它排好的例行工作（重建、分診、清理…）會找不到這顆，要等人補回來。')}</li>
              <li>{t('確定是你要刪的才按「仍要全部刪除」；其餘的已經刪掉，對話紀錄會保留，之後可從通知的「復原」還原。')}</li>
            </ul>
          </>
        ) : null
      }
      confirmLabel={t('仍要全部刪除')}
      danger
      width={400}
      onCancel={cancelBatch}
      onConfirm={() => void confirmBatch()}
    />
    <ConfirmDialog
      open={ask !== null}
      title={t('這是 AGM 的 Bot')}
      body={
        ask ? (
          <>
            {ask.role
              ? t.x('{name} 是 AGM 的 bot（{role}）。', { name: <strong>{ask.name}</strong>, role: ask.role })
              : t.x('{name} 是 AGM 的 bot。', { name: <strong>{ask.name}</strong> })}
            <ul className="confirm-choices">
              <li>{t('刪了 AGM 可能會壞：它排好的例行工作（重建、分診、清理…）會找不到這顆，要等人補回來。')}</li>
              <li>{t('確定是你要刪的才按「仍要刪除」；對話紀錄會保留，之後可從通知的「復原」還原。')}</li>
            </ul>
          </>
        ) : null
      }
      confirmLabel={t('仍要刪除')}
      danger
      width={400}
      onCancel={cancel}
      onConfirm={() => {
        if (!ask) return
        const botId = ask.botId
        cancel()
        void removeBot(botId, { confirmSupervisor: true })
      }}
    />
    </>
  )
}
