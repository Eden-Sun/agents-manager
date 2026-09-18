import { useState, type ReactNode } from 'react'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { updateBatchCounts } from '../lib/updateBatch'
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
 *
 * 2026-09-11 使用者：**chip 版在「批次那顆蓋得到它」的時候不畫**。額度列最左邊那顆綠色
 * `⌃⌃ N`（`UpdateQuotaChip`）按下去會一次重啟 `updateBatchCounts(...).ready` 裡的每一顆；
 * 當前這顆落在那份名單裡時，標題列再放一顆等於同一件事講兩次，還吃掉第一排 118px 的寬度
 * （標題列第一排的寬度是稀缺資源，見 `styles.css` 的「標題列的收縮優先序」）。
 * 批次**蓋不到**的三種留著——只有這顆能單獨重啟它們：在忙（`busy` 名單，批次會跳過）、
 * 不是 `managed_by === 'user'`（子 agent、team 成員）、不是 claude。判斷全部交給
 * `updateBatch.ts` 那份與 daemon 一字不差的規則，這裡不另外寫一套。
 */
export function UpdateBadge({ botId, variant = 'chip' }: { botId: string; variant?: 'chip' | 'dot' | 'inline' }) {
  const run = useStore((s) => s.runs[botId] ?? null)
  const botName = useStore((s) => s.bots.find((b) => b.id === botId)?.name ?? '這個 Bot')
  const botKind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? 'claude')
  const host = useStore((s) => projectHostName(s, s.bots.find((b) => b.id === botId)?.project_id ?? null))
  const runningVersion = useStore((s) => s.runs[botId]?.status?.version ?? null)
  const restartBot = useStore((s) => s.restartBot)
  const notify = useStore((s) => s.notify)
  const [restarting, setRestarting] = useState(false)
  const [confirming, setConfirming] = useState(false)

  // selector 回**純布林**：`updateBatchCounts` 每次都回新的陣列，回陣列或包成物件回去連
  // `useShallow` 都擋不住，React 會噴 `getSnapshot should be cached`（見 `UpdateQuotaChip.tsx`）。
  const coveredByBatch = useStore((s) =>
    updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.some((b) => b.botId === botId),
  )

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

  // `inline`：context bar 版本號右邊那顆（2026-09-12 使用者：「可升級就出現在版本號右邊以方便點選」）。
  // 那一列是看版本的地方，批次蓋不蓋得到都畫——這裡不是標題列第一排，不搶寬度。
  if (variant === 'inline') {
    return (
      <>
        <button
          type="button"
          className={`sl-update${restarting ? ' busy' : ''}`}
          disabled={restarting}
          aria-label={`${botName}：${botKind} 有更新，點一下先看新版改了什麼，確認後重啟`}
          title={`${notice}\n點一下先看新版改了什麼，確認後重啟這個 bot（session 會 --resume）${busy ? '\n它正在忙，重啟會打斷這一回合' : ''}`}
          onClick={() => setConfirming(true)}
        >
          {restarting ? '重啟中…' : <><UpgradeIcon size={9} /> 升級</>}
        </button>
        {confirmDialog}
      </>
    )
  }

  // 批次那顆蓋得到它就不畫（理由見檔頭）。已經按開確認框或正在重啟時仍然要留著，
  // 不然狀態一變成 `ready` 會把使用者面前的對話框整個抽走。
  if (coveredByBatch && !confirming && !restarting) return null

  return (
    <>
      <button
        type="button"
        className={`update-badge${restarting ? ' busy' : ''}`}
        disabled={restarting}
        // 條上只寫「誰有更新」，「點下去會發生什麼」留在這裡——第一排的寬度要留給名字。
        aria-label={`${botName}：${botKind} 有更新，點一下先看新版改了什麼，確認後重啟`}
        title={`${notice}\n點一下先看新版改了什麼，確認後重啟這個 bot（session 會 --resume）${busy ? '\n它正在忙，重啟會打斷這一回合' : ''}`}
        onClick={() => setConfirming(true)}
      >
        {restarting ? '重啟中…' : <><UpgradeIcon /> <span className="update-badge-text">{botKind} 有更新</span></>}
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
