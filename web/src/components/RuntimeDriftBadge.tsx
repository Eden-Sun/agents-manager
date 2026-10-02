import { useState } from 'react'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import * as api from '../api'
import { driftLine, isFastOnlyDrift, runtimeDrift } from '../lib/runtimeDrift'

/**
 * 「設定改了，但 bot 還跑在舊值上」的常駐 chip，點下去重啟（SPEC §4.4a）。
 * 啟動相關欄位只在啟動時吃得到，比對的是 daemon 記下的 `run.runtime_*` 與設定；
 * daemon 的 `bot.needs_restart` 仍是全域重啟狀態的唯一來源，這顆只列已知欄位細節。
 */
export function RuntimeDriftBadge({ botId }: { botId: string }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const run = useStore((s) => s.runs[botId] ?? null)
  const restartBot = useStore((s) => s.restartBot)
  const notify = useStore((s) => s.notify)
  const [restartingLocal, setRestarting] = useState(false)
  // 這顆 bot 在別處（設定面板、更新徽章、別的分頁送的請求）已經在重啟：store 的 busy 旗標，所有重啟鈕共用。
  const restartingShared = useStore((s) => Boolean(s.busy[`restart:${botId}`]))
  const restarting = restartingLocal || restartingShared
  const [confirming, setConfirming] = useState(false)
  // codex 的 fast（#393）：按下去當場套用，回合中也是（#712）；輸入框有字才排到回合結束，這時徽章改寫「回合結束後套用」。
  const refreshState = useStore((s) => s.refreshState)
  const [applying, setApplying] = useState(false)

  const drift = runtimeDrift(bot, run)
  // daemon 說要重啟（`needs_restart`），但改的是「已知落差欄位」以外的東西（人設、args、env…）：沒有落差明細也要留徽章，
  // 不然只有設定面板開著時才看得到「要重啟」（#353：這個旗標從資料算，不依賴 PATCH 回應）。
  const unlisted = Boolean(bot?.needs_restart) && !drift.length
  if (!bot || (!drift.length && !unlisted)) return null

  const busy = run?.agent_status === 'working' || run?.agent_status === 'blocked'
  const fastOnly = !unlisted && isFastOnlyDrift(bot.kind, drift)
  const driftLabels = unlisted ? '設定已改' : drift.map((d) => d.label).join('、')
  const driftTitle = unlisted ? '人設、args、env 等設定改了，執行中的 CLI 只在啟動時載入' : drift.map(driftLine).join('\n')
  const deferred = bot.live_apply_deferred
  // 重送同一個 fast 值：冪等，daemon 會當場套用（回合中也是）、碰不得畫面就排到回合結束、都不行才回 needs_restart。
  const applyFast = async () => {
    setApplying(true)
    try {
      const res = await api.patchBot(botId, { fast: bot.fast })
      await refreshState()
      const la = res.live_apply
      if (la?.applied) {
        notify('info', `${bot.name} 的 fast 已套用，沒有重啟`)
      } else if (la?.deferred) {
        notify('info', `${bot.name} 回合結束後會自動套用 fast`)
      } else {
        // 套不上（不是忙、是別的原因）：退回重啟，維持原本的確認流程。
        if (busy) setConfirming(true)
        else restart()
      }
    } catch (e) {
      notify('error', `fast 套用失敗：${e instanceof Error ? e.message : String(e)}`)
    } finally {
      setApplying(false)
    }
  }
  const restart = () => {
    setConfirming(false)
    setRestarting(true)
    void restartBot(botId).then((ok) => {
      setRestarting(false)
      if (ok) notify('info', `${bot.name} 已用新的設定重新啟動`)
    })
  }

  return (
    <>
      <button
        type="button"
        className={`update-badge drift-badge${restarting || applying ? ' busy' : ''}`}
        disabled={restarting || applying || deferred}
        title={
          deferred
            ? `${driftTitle}\n回合結束後自動套用`
            : fastOnly
            ? `${driftTitle}\ncodex 的 fast 可以當場切換，不用重啟${busy ? '\n回合中也直接切；輸入框有字才排到回合結束' : ''}`
            : `${driftTitle}\n${bot.kind} 只有啟動時吃得到這些設定，重啟才會換過去${busy ? '\n它正在忙，會先問一句' : ''}`
        }
        onClick={() => {
          if (fastOnly) void applyFast()
          else if (busy) setConfirming(true)
          else restart()
        }}
      >
        {restarting
          ? '重啟中…'
          : applying
            ? '套用中…'
            : deferred
              ? `⏳ ${driftLabels}待套用`
              : fastOnly
              ? '⟳ fast 當場套用'
              : `⟳ ${driftLabels}需重啟`}
      </button>
      <ConfirmDialog
        open={confirming}
        title="現在重啟套用新設定？"
        body={
          <>
            <strong>{bot.name}</strong> 正在忙，現在重啟會打斷這一回合。重啟後換成設定的值：
            {drift.map((d) => `${d.label} ${d.running} → ${d.configured}`).join('、')}。
          </>
        }
        confirmLabel="重啟套用"
        danger
        width={360}
        onCancel={() => setConfirming(false)}
        onConfirm={restart}
      />
    </>
  )
}
