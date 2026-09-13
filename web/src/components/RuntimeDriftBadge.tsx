import { useState } from 'react'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { driftLine, runtimeDrift } from '../lib/runtimeDrift'

/**
 * 「設定改了，但 bot 還跑在舊值上」的常駐 chip，點下去重啟（SPEC §4.4a）。
 * codex 的模型／強度／fast 只在啟動時吃得到，比對的是 `run.runtime_*` 與設定。
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
