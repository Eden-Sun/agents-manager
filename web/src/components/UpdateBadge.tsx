import { useState } from 'react'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'

/**
 * claude 下載好新版之後，只會在 pane 最底下那行印
 * `✔ Update installed · Restart to update` 就沒別的動靜了——使用者要一路點進終端才看得到。
 * daemon 把那句讀出來掛在 run 上（`runs.update_notice`），這顆 chip 就把它擺到 header 上，
 * 而且點得下去：套用更新的方式就是重啟，走既有的 `POST /api/bots/{id}/restart`。
 *
 * 忙的時候（`working` / `blocked`）先問一句——重啟會打斷這一回合；`idle` 直接重啟。
 */
export function UpdateBadge({ botId }: { botId: string }) {
  const run = useStore((s) => s.runs[botId] ?? null)
  const botName = useStore((s) => s.bots.find((b) => b.id === botId)?.name ?? '這個 Bot')
  const restartBot = useStore((s) => s.restartBot)
  const notify = useStore((s) => s.notify)
  const [restarting, setRestarting] = useState(false)
  const [confirming, setConfirming] = useState(false)

  const notice = run?.update_notice ?? null
  if (!notice) return null

  const busy = run?.agent_status === 'working' || run?.agent_status === 'blocked'
  const restart = () => {
    setConfirming(false)
    setRestarting(true)
    void restartBot(botId).then((ok) => {
      setRestarting(false)
      if (ok) notify('info', `${botName} 已用新版 claude 重新啟動`)
    })
  }

  return (
    <>
      <button
        type="button"
        className={`update-badge${restarting ? ' busy' : ''}`}
        disabled={restarting}
        title={`${notice}\n重啟這個 bot 會用新版 claude 接著跑（session 會 --resume）${busy ? '\n它正在忙，會先問一句' : ''}`}
        onClick={() => (busy ? setConfirming(true) : restart())}
      >
        {restarting ? '重啟中…' : <>⬆ <span className="update-badge-text">有更新 · 重啟套用</span></>}
      </button>
      <ConfirmRestart open={confirming} name={botName} onCancel={() => setConfirming(false)} onConfirm={restart} />
    </>
  )
}

/** 忙碌時才出現的那一問。獨立成內部元件，讓上面那個 chip 維持一眼看得完。 */
function ConfirmRestart({
  open,
  name,
  onCancel,
  onConfirm,
}: {
  open: boolean
  name: string
  onCancel: () => void
  onConfirm: () => void
}) {
  return (
    <ConfirmDialog
      open={open}
      title="現在重啟套用更新？"
      body={
        <>
          <strong>{name}</strong> 正在忙，現在重啟會打斷這一回合。新版 claude 會 <code>--resume</code>{' '}
          接著同一個 session 跑，但這一輪沒說完的話會斷在那裡。
        </>
      }
      confirmLabel="重啟套用"
      danger
      width={360}
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  )
}
