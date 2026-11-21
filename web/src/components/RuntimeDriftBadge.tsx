import { useState } from 'react'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import * as api from '../api'
import { isChildBot } from '../lib/updateBatch'
import { driftLine, isFastOnlyDrift, runtimeDrift } from '../lib/runtimeDrift'
import { useT } from '../lib/i18n'

export function RuntimeDriftBadge({ botId }: { botId: string }) {
  const t = useT()
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const run = useStore((s) => s.runs[botId] ?? null)
  const restartBot = useStore((s) => s.restartBot)
  const notify = useStore((s) => s.notify)
  const [restartingLocal, setRestarting] = useState(false)
  const restartingShared = useStore((s) => Boolean(s.busy[`restart:${botId}`]))
  const restarting = restartingLocal || restartingShared
  const [confirming, setConfirming] = useState(false)
  const refreshState = useStore((s) => s.refreshState)
  const [applying, setApplying] = useState(false)

  const drift = runtimeDrift(bot, run)
  const unlisted = Boolean(bot?.needs_restart) && !drift.length
  if (!bot || (!drift.length && !unlisted)) return null

  const busy = run?.agent_status === 'working' || run?.agent_status === 'blocked'
  const fastOnly = !unlisted && isFastOnlyDrift(bot.kind, drift)
  const driftLabels = unlisted ? t('設定已改') : drift.map((d) => d.label).join(t('、'))
  const driftTitle = unlisted ? t('人設、args、env 等設定改了，執行中的 CLI 只在啟動時載入') : drift.map(driftLine).join('\n')
  const deferred = bot.live_apply_deferred
  const applyFast = async () => {
    setApplying(true)
    try {
      const res = await api.patchBot(botId, { fast: bot.fast })
      await refreshState()
      const la = res.live_apply
      if (la?.applied) {
        notify('info', t('{name} 的 fast 已套用，沒有重啟', { name: bot.name }))
      } else if (la?.deferred) {
        notify('info', t('{name} 回合結束後會自動套用 fast', { name: bot.name }))
      } else {
        if (busy) setConfirming(true)
        else restart()
      }
    } catch (e) {
      notify('error', t('fast 套用失敗：{reason}', { reason: e instanceof Error ? e.message : String(e) }))
    } finally {
      setApplying(false)
    }
  }
  const child = isChildBot(bot)
  const restart = () => {
    if (child) {
      setConfirming(false)
      notify('info', t('{name} 是子 agent，設定要重啟才生效，由父 Bot 安排重啟', { name: bot.name }))
      return
    }
    setConfirming(false)
    setRestarting(true)
    void restartBot(botId).then((ok) => {
      setRestarting(false)
      if (ok) notify('info', t('{name} 已用新的設定重新啟動', { name: bot.name }))
    })
  }

  if (child && !fastOnly && !deferred) {
    return (
      <span
        className="update-badge drift-badge passive"
        title={`${driftTitle}\n${t('設定要重啟才生效，由父 Bot 安排重啟')}`}
      >
        {t('{labels}待父 Bot 重啟', { labels: driftLabels })}
      </span>
    )
  }

  return (
    <>
      <button
        type="button"
        className={`update-badge drift-badge${restarting || applying ? ' busy' : ''}`}
        disabled={restarting || applying || deferred}
        title={
          deferred
            ? `${driftTitle}\n${t('回合結束後自動套用')}`
            : fastOnly
            ? `${driftTitle}\n${t('codex 的 fast 可以當場切換，不用重啟')}${busy ? t('\n回合中也直接切；輸入框有字才排到回合結束') : ''}`
            : `${driftTitle}\n${t('{kind} 只有啟動時吃得到這些設定，重啟才會換過去', { kind: bot.kind })}${busy ? t('\n它正在忙，會先問一句') : ''}`
        }
        onClick={() => {
          if (fastOnly) void applyFast()
          else if (busy) setConfirming(true)
          else restart()
        }}
      >
        {restarting
          ? t('重啟中…')
          : applying
            ? t('套用中…')
            : deferred
              ? t('⏳ {labels}待套用', { labels: driftLabels })
              : fastOnly
              ? t('⟳ fast 當場套用')
              : t('⟳ {labels}需重啟', { labels: driftLabels })}
      </button>
      <ConfirmDialog
        open={confirming}
        title={t('現在重啟套用新設定？')}
        body={
          <>
            {t.x('{bot} 正在忙，現在重啟會打斷這一回合。重啟後換成設定的值：{changes}。', {
              bot: <strong>{bot.name}</strong>,
              changes: drift.map((d) => `${d.label} ${d.running} → ${d.configured}`).join(t('、')),
            })}
          </>
        }
        confirmLabel={t('重啟套用')}
        danger
        width={360}
        onCancel={() => setConfirming(false)}
        onConfirm={restart}
      />
    </>
  )
}
