import { useShallow } from 'zustand/react/shallow'
import { composerState, useStore } from '../store/store'
import { visibleSuggestion } from '../store/promptSuggestion'
import { useT } from '../lib/i18n'
import './suggestionBar.css'

export function SuggestionBar({ botId, attachments }: { botId: string; attachments: number }) {
  const t = useT()
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
    <div className="suggestion-bar" role="group" aria-label={t('建議的下一句')}>
      <button
        type="button"
        className="suggestion-bar-btn"
        disabled={busy}
        title={`${text}\n\n${t('點一下就送出這句（等同在終端按 Tab 收下、再按 Enter）')}`}
        onClick={() => void accept(botId)}
      >
        {}
        <span className="suggestion-bar-keys">{t('claude 建議：')}</span>
        <span className="suggestion-bar-text">{busy ? t('送出中…') : text}</span>
        {busy ? null : <span className="suggestion-bar-hint">{t('點擊送出')}</span>}
      </button>
    </div>
  )
}
