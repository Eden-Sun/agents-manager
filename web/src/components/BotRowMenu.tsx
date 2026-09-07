import { useState } from 'react'
import { anchorOf, useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { HeadMoreMenu } from './HeadMoreMenu'

/**
 * bot 列尾端的動作，收成一顆 `⋯`。
 *
 * 原本是四顆 icon 疊在名字上、hover 才浮出來：側欄一窄就蓋掉名字，觸控裝置沒有 hover
 * 只能靠 `@media (hover: none)` 另開一套排法——同一件事兩份規則，寬度一變就有一邊壞掉。
 * 一顆固定 28px 的按鈕不佔名字的寬、不需要 hover 才看得到，任何寬度與任何輸入方式都一樣。
 * 常用的「設定」仍是選單第一項，破壞性的刪除照 UI-DECISIONS 排在最後並且走確認框。
 */
export function BotRowMenu({ botId, compact }: { botId: string; compact?: boolean }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId))
  const active = useStore((s) => {
    const r = s.runs[botId]
    return r != null && r.state !== 'stopped' && r.state !== 'exited'
  })
  const busyStart = useStore((s) => Boolean(s.busy[`start:${botId}`]))
  const busyClone = useStore((s) => Boolean(s.busy[`clone:${botId}`]))
  const openSettings = useStore((s) => s.openSettings)
  const startBot = useStore((s) => s.startBot)
  const cloneBot = useStore((s) => s.cloneBot)
  const removeBot = useStore((s) => s.removeBot)
  const [deleteOpen, setDeleteOpen] = useState(false)
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
        {compact ? null : (
          <button
            type="button"
            className="head-menu-item"
            role="menuitem"
            disabled={busyClone}
            title="同 kind、模型、身份、人設"
            onClick={() => void cloneBot(botId)}
          >
            開同類分身並啟動
          </button>
        )}
        <button type="button" className="head-menu-item danger" role="menuitem" onClick={() => setDeleteOpen(true)}>
          刪除…
        </button>
      </HeadMoreMenu>

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
