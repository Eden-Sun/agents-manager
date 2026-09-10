import { useState, type ReactNode } from 'react'
import { projectHostName, useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'

/**
 * claude 下載好新版之後，只會在 pane 最底下那行印
 * `✔ Update installed · Restart to update` 就沒別的動靜了——使用者要一路點進終端才看得到。
 * daemon 把那句讀出來掛在 run 上（`runs.update_notice`），這顆 chip 就把它擺到 header 上，
 * 而且點得下去：套用更新的方式就是重啟，走既有的 `POST /api/bots/{id}/restart`。
 *
 * 2026-09-10：不管忙不忙都先開確認框，框裡先列新版 changelog（抓不到就寫「找不到」），
 * 使用者看過按了才重啟；忙的時候多一句「會打斷這一回合」。
 *
 * `variant="dot"` 是側欄 bot 列上那顆掛在 kind icon 右上角的小 ⌃⌃（2026-09-11 使用者：
 * 「click to update latest」）。它本來只是提示，要套用得先切到那顆 bot 再點 header 的
 * chip——但看到記號的當下人就在側欄。同一顆元件、同一條確認流程，只是換個外觀。
 */
export function UpdateBadge({ botId, variant = 'chip' }: { botId: string; variant?: 'chip' | 'dot' }) {
  const run = useStore((s) => s.runs[botId] ?? null)
  const botName = useStore((s) => s.bots.find((b) => b.id === botId)?.name ?? '這個 Bot')
  const botKind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? 'claude')
  const host = useStore((s) => projectHostName(s, s.bots.find((b) => b.id === botId)?.project_id ?? null))
  const runningVersion = useStore((s) => s.runs[botId]?.status?.version ?? null)
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

  const confirmDialog = (
    <ConfirmRestart
      open={confirming}
      name={botName}
      busy={busy}
      changelog={confirming ? <UpdateChangelog kind={botKind} host={host} from={runningVersion} /> : null}
      onCancel={() => setConfirming(false)}
      onConfirm={restart}
    />
  )

  if (variant === 'dot') {
    return (
      <>
        <button
          type="button"
          className={`bot-update-dot${restarting ? ' busy' : ''}`}
          disabled={restarting}
          aria-label={`${botName}：有更新，點一下套用`}
          title={`${notice}｜點一下先看新版改了什麼，確認後重啟這個 bot（session 會 --resume）${busy ? '\n它正在忙，重啟會打斷這一回合' : ''}`}
          // 這顆疊在 bot 列上，列本身點下去是「切換到這個 bot」——按更新不該順便換畫面。
          onClick={(e) => {
            e.stopPropagation()
            setConfirming(true)
          }}
        >
          <UpgradeIcon size={8} />
        </button>
        {confirmDialog}
      </>
    )
  }

  return (
    <>
      <button
        type="button"
        className={`update-badge${restarting ? ' busy' : ''}`}
        disabled={restarting}
        title={`${notice}\n點一下先看新版改了什麼，確認後重啟這個 bot（session 會 --resume）${busy ? '\n它正在忙，重啟會打斷這一回合' : ''}`}
        onClick={() => setConfirming(true)}
      >
        {restarting ? '重啟中…' : <><UpgradeIcon /> <span className="update-badge-text">有更新 · 重啟套用</span></>}
      </button>
      {confirmDialog}
    </>
  )
}

/** 那一問：changelog 在上，忙碌警語在下。獨立成內部元件，讓上面那個 chip 維持一眼看得完。 */
function ConfirmRestart({
  open,
  name,
  busy,
  changelog,
  onCancel,
  onConfirm,
}: {
  open: boolean
  name: string
  busy: boolean
  changelog: ReactNode
  onCancel: () => void
  onConfirm: () => void
}) {
  return (
    <ConfirmDialog
      open={open}
      title="套用 claude 更新？"
      body={
        <>
          {changelog}
          {busy ? (
            <p>
              <strong>{name}</strong> 正在忙，現在重啟會打斷這一回合。新版 claude 會 <code>--resume</code>{' '}
              接著同一個 session 跑，但這一輪沒說完的話會斷在那裡。
            </p>
          ) : (
            <p>
              重啟 <strong>{name}</strong> 會用新版 claude 接著跑（session 會 <code>--resume</code>，上下文不會掉）。
            </p>
          )}
        </>
      }
      confirmLabel="重啟套用"
      danger={busy}
      width={440}
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  )
}
