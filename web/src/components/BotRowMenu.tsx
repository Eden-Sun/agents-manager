import { useState } from 'react'
import { anchorOf, useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { HeadMoreMenu } from './HeadMoreMenu'

/**
 * bot 列尾端的動作收成一顆固定寬的 `⋯`：hover 浮出的 icon 會蓋名字、觸控裝置又得另寫一套。
 * 破壞性的刪除照 UI-DECISIONS 排最後並走確認框。
 */
export function BotRowMenu({ botId, compact }: { botId: string; compact?: boolean }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId))
  const active = useStore((s) => {
    const r = s.runs[botId]
    return r != null && r.state !== 'stopped' && r.state !== 'exited'
  })
  const busyStart = useStore((s) => Boolean(s.busy[`start:${botId}`]))
  const busyClone = useStore((s) => Boolean(s.busy[`clone:${botId}`]))
  const busyFork = useStore((s) => Boolean(s.busy[`fork:${botId}`]))
  // 接續得了才給 fork：daemon 取的是最近一次記到的 native session，這裡用最近一個 run 當提示。
  const canFork = useStore((s) => Boolean(s.runs[botId]?.native_session_id))
  const openSettings = useStore((s) => s.openSettings)
  const startBot = useStore((s) => s.startBot)
  const cloneBot = useStore((s) => s.cloneBot)
  const forkBot = useStore((s) => s.forkBot)
  const promoteBot = useStore((s) => s.promoteBot)
  const busyPromote = useStore((s) => Boolean(s.busy[`promote:${botId}`]))
  const removeBot = useStore((s) => s.removeBot)
  const [deleteOpen, setDeleteOpen] = useState(false)
  const [cloneOpen, setCloneOpen] = useState(false)
  const [promoteOpen, setPromoteOpen] = useState(false)
  if (!bot) return null

  return (
    <span className="bot-actions" onClick={(e) => e.stopPropagation()}>
      <HeadMoreMenu label={`${bot.name} 的操作`}>
        <button
          type="button"
          className="head-menu-item"
          role="menuitem"
          onClick={(e) => openSettings(botId, anchorOf(e.currentTarget))}
        >
          設定…
        </button>
        {active ? null : (
          <button
            type="button"
            className="head-menu-item"
            role="menuitem"
            disabled={busyStart}
            onClick={() => void startBot(botId)}
          >
            啟動
          </button>
        )}
        {/* 分身只給頂層 bot（child 列是 compact）：child 的 pane 與帳號環境是母 agent 開的。 */}
        {compact || bot.managed_by === 'child' ? null : (
          <button
            type="button"
            className="head-menu-item"
            role="menuitem"
            disabled={busyClone || busyFork}
            title="同 kind、模型、身份、人設；可選擇接續目前的對話"
            onClick={() => setCloneOpen(true)}
          >
            開同類分身…
          </button>
        )}
        {/* 子 agent 升級成頂層 bot（issue #248）：daemon 只收 claude、本機、沒有孫 agent 的 child。 */}
        {bot.managed_by === 'child' && bot.kind === 'claude' ? (
          <button
            type="button"
            className="head-menu-item"
            role="menuitem"
            disabled={busyPromote}
            title="保留同一段對話，移出母 bot、進 config.toml，之後由你直接對話"
            onClick={() => setPromoteOpen(true)}
          >
            升級成頂層…
          </button>
        ) : null}
        <button type="button" className="head-menu-item danger" role="menuitem" onClick={() => setDeleteOpen(true)}>
          刪除…
        </button>
      </HeadMoreMenu>

      <ConfirmDialog
        open={cloneOpen}
        title="開同類分身"
        body={
          <>
            照 <strong>{bot.name}</strong> 的設定（kind、模型、身份、人設）開一顆新的並啟動。要接續它目前的對話嗎？
            <ul className="confirm-choices">
              <li>
                <strong>接續對話（fork）</strong>：帶著它到目前為止的完整脈絡，之後各走各的。
              </li>
              <li>
                <strong>全新對話</strong>：從零開始。
              </li>
            </ul>
            {canFork ? null : <p className="confirm-note">這顆還沒有可以接續的對話，只能開全新的。</p>}
          </>
        }
        secondaryLabel="全新對話"
        onSecondary={() => {
          setCloneOpen(false)
          void cloneBot(botId)
        }}
        confirmLabel="接續對話（fork）"
        confirmDisabled={!canFork}
        width={380}
        onCancel={() => setCloneOpen(false)}
        onConfirm={() => {
          setCloneOpen(false)
          void forkBot(botId)
        }}
      />

      <ConfirmDialog
        open={promoteOpen}
        title="升級成頂層 bot"
        body={
          <>
            把子 agent <strong>{bot.name}</strong> 升級成頂層 bot：會先停掉它、把對話檔複製到專案目錄，再以同一段對話重新啟動。
            <ul className="confirm-choices">
              <li>保留同一段 claude 對話（<code>--resume</code>），之後不再縮排在母 bot 底下。</li>
              <li>任何一步失敗就整個收回，不會留下兩顆。</li>
            </ul>
          </>
        }
        confirmLabel="升級"
        width={380}
        onCancel={() => setPromoteOpen(false)}
        onConfirm={() => {
          setPromoteOpen(false)
          void promoteBot(botId)
        }}
      />

      <ConfirmDialog
        open={deleteOpen}
        title="刪除 Bot"
        body={
          <>
            確定刪除 <strong>{bot.name}</strong>？會停止並關閉它的終端 pane，設定從 config.toml 移除；對話紀錄會保留。
          </>
        }
        confirmLabel="刪除"
        danger
        width={340}
        onCancel={() => setDeleteOpen(false)}
        onConfirm={() => {
          setDeleteOpen(false)
          void removeBot(botId)
        }}
      />
    </span>
  )
}
