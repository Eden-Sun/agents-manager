import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'

/** 刪 AGM 的 bot 的第二次確認（issue #406）。第一次刪除被 daemon 409 `supervisor_owned` 擋下時才出現；
 *  取消什麼都不做，確認才帶 `?confirm=supervisor` 重送。掛在 App 根層：兩個刪除入口（側欄 ⋯、Bot 設定）共用。 */
export function AgmDeleteConfirm() {
  const ask = useStore((s) => s.agmDeleteAsk)
  const cancel = useStore((s) => s.cancelAgmDelete)
  const removeBot = useStore((s) => s.removeBot)
  return (
    <ConfirmDialog
      open={ask !== null}
      title="這是 AGM 的 Bot"
      body={
        ask ? (
          <>
            <strong>{ask.name}</strong> 是 AGM 的 bot{ask.role ? <>（{ask.role}）</> : null}。
            <ul className="confirm-choices">
              <li>刪了 AGM 可能會壞：它排好的例行工作（重建、分診、清理…）會找不到這顆，要等人補回來。</li>
              <li>確定是你要刪的才按「仍要刪除」；對話紀錄會保留，之後可從通知的「復原」還原。</li>
            </ul>
          </>
        ) : null
      }
      confirmLabel="仍要刪除"
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
  )
}
