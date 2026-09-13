import { useState } from 'react'
import { usePaneKeys } from '../hooks/usePaneKeys'
import { parseCodexUpdatePrompt } from '../lib/codexUpdatePrompt'
import { projectHostName, useStore } from '../store/store'
import { UpdateChangelog } from './UpdateChangelog'

/**
 * codex TUI 升級提示（`Update available! 0.153.4 -> 0.154.0`）上的「先看改了什麼」。
 * `BlockedModal` 與 `BlockedPanel` 都要有（2026-09-10 使用者：只有全畫面才有）。
 */
export function CodexUpdateHint({
  botId,
  text,
  onAnswered,
}: {
  botId: string
  text: string | null | undefined
  /** 送出答案後叫一次，讓外面那張終端快照立刻重抓（不必等下一秒的輪詢）。 */
  onAnswered?: () => void
}) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const host = useStore((s) => projectHostName(s, bot?.project_id ?? null))
  const press = usePaneKeys(botId, onAnswered)
  const [showLog, setShowLog] = useState(false)
  const update = bot?.kind === 'codex' ? parseCodexUpdatePrompt(text) : null
  if (!update) return null
  return (
    <div className="blocked-modal-update">
      <button type="button" className="mini-btn" onClick={() => setShowLog((v) => !v)}>
        {showLog ? '收起 changelog' : '先看改了什麼'}
      </button>
      <button type="button" className="mini-btn" title="送出 1：現在更新（codex 會自己跑安裝指令）" onClick={() => press(['1'])}>
        1 更新
      </button>
      <button type="button" className="mini-btn" title="送出 2：這次跳過，下次還會問" onClick={() => press(['2'])}>
        2 跳過
      </button>
      <button type="button" className="mini-btn" title="送出 3：跳過這一版，出更新的版本才再問" onClick={() => press(['3'])}>
        3 跳過這版
      </button>
      <span className="hint">
        codex {update.from ? `${update.from} → ${update.to}` : update.to}：按 1 更新前先看新版的 release notes。
      </span>
      {showLog ? <UpdateChangelog kind="codex" host={host} from={update.from} to={update.to} /> : null}
    </div>
  )
}
