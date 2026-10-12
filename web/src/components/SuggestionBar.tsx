import { useShallow } from 'zustand/react/shallow'
import { composerState, useStore } from '../store/store'
import { visibleSuggestion } from '../store/promptSuggestion'
import './suggestionBar.css'

/**
 * claude 回合結束後輸入框裡那句灰字「建議下一句」（終端 Tab 收下、Enter 送出）。顯示在輸入列上方一行（灰色、單行截斷），
 * 點一下就送：daemon 對 pane 送 Tab 再送 Enter，跟終端一模一樣，不是把字打進去（2026-10-03 使用者）。
 * 輸入框裡有字、有附件、回合在跑、框裡卡著草稿時不顯示；送出中按鈕禁用，失敗的原因由 store 跳通知。
 */
export function SuggestionBar({ botId, attachments }: { botId: string; attachments: number }) {
  const draftKey = `bot:${botId}` as const
  const text = useStore((s) =>
    visibleSuggestion({
      kind: s.bots.find((b) => b.id === botId)?.kind,
      run: s.runs[botId],
      draft: s.drafts[draftKey] ?? '',
      attachments,
      composerBusy: composerState(s, botId).disabled || composerState(s, botId).queued,
      draftBarOpen: Boolean(s.composerDrafts[botId]),
    }),
  )
  const busy = useStore(useShallow((s) => Boolean(s.busy[`suggest:${botId}`])))
  const accept = useStore((s) => s.acceptSuggestion)
  if (!text) return null
  return (
    <div className="suggestion-bar" role="group" aria-label="建議的下一句">
      <button
        type="button"
        className="suggestion-bar-btn"
        disabled={busy}
        title={`${text}\n\n點一下就送出這句（等同在終端按 Tab 收下、再按 Enter）`}
        onClick={() => void accept(botId)}
      >
        {/* 不寫「Tab ⏎」：那是終端的按鍵，網頁上看不懂（2026-10-12 使用者：「這不叫 tab，提示字為 claude 建議：… 點擊送出」）。 */}
        <span className="suggestion-bar-keys">claude 建議：</span>
        <span className="suggestion-bar-text">{busy ? '送出中…' : text}</span>
        {busy ? null : <span className="suggestion-bar-hint">點擊送出</span>}
      </button>
    </div>
  )
}
