import { useState } from 'react'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { driftLine, runtimeDrift } from '../lib/runtimeDrift'

/**
 * 「設定改了，但這顆 bot 還跑在舊的值上」——標出來，並且點得下去（SPEC §4.4a）。
 *
 * codex 的模型／強度／fast 只有**啟動時**吃得到（`-m`、`-c model_reasoning_effort=…`、
 * `-c service_tier="priority"`）：它的 TUI 沒有 `/model`、`/effort` 可以讓 daemon 當場送進去，
 * 所以 `PATCH /api/bots/{id}` 只會回 `needs_restart: true`。在這顆 badge 之前，那件事只存在
 * 於一則會自己消失的 toast 裡，之後 UI 就一路顯示新設定——使用者在 AG Man 上看到
 * `gpt-5.6-luna-High`，終端裡 codex 自己印的卻是 `gpt-5.6-luna xhigh fast`。
 *
 * daemon 現在把 run 真正啟動時的 argv 讀回來（`run.runtime_*`），這裡拿它跟設定比，不一致
 * 就在標題列上留一顆常駐的 chip，寫明哪個欄位、實際在跑什麼、設定成什麼；按下去就是重啟
 * （既有的 `POST /api/bots/{id}/restart`），忙的時候先問一句。
 */
export function RuntimeDriftBadge({ botId }: { botId: string }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const run = useStore((s) => s.runs[botId] ?? null)
  const restartBot = useStore((s) => s.restartBot)
  const notify = useStore((s) => s.notify)
  const [restarting, setRestarting] = useState(false)
  const [confirming, setConfirming] = useState(false)

  const drift = runtimeDrift(bot, run)
  if (!bot || !drift.length) return null

  const busy = run?.agent_status === 'working' || run?.agent_status === 'blocked'
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
        className={`update-badge drift-badge${restarting ? ' busy' : ''}`}
        disabled={restarting}
        title={`${drift.map(driftLine).join('\n')}\n${bot.kind} 只有啟動時吃得到這些設定，重啟才會換過去${busy ? '\n它正在忙，會先問一句' : ''}`}
        onClick={() => (busy ? setConfirming(true) : restart())}
      >
        {restarting ? '重啟中…' : `⟳ ${drift.map((d) => d.label).join('、')}需重啟`}
      </button>
      <ConfirmDialog
        open={confirming}
        title="現在重啟套用新設定？"
        body={
          <>
            <strong>{bot.name}</strong> 正在忙，現在重啟會打斷這一回合。重啟後才會換成
            {drift.map((d) => `${d.label} ${d.configured}`).join('、')}。
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
