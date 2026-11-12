import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { SafeMarkdown } from './SafeMarkdown'
import { isLocalTranslation, translationModelName, usd } from './messageTranslationConstants'

/**
 * 譯文下方那行：`(by <模型短名> $<金額>)`（使用者 2026-11-12 裁示）。快取命中也照樣顯示原本的金額，
 * 不另標「未扣費」；OpenCC 只寫 `(by OpenCC)`；金額不明時只寫模型名。
 */
function costLine(t: ReturnType<typeof useT>, st: { cost: number | null; model: string }): string {
  const model = translationModelName(st.model)
  if (isLocalTranslation(st.model) || st.cost === null) return t('(by {model})', { model })
  return t('(by {model} {cost})', { model, cost: usd(st.cost) })
}

/**
 * 訊息翻譯按鈕與譯文呈現元件。
 * 讓訊息翻譯具備 React Refresh 邊界，改動只熱替換本身與使用處，不觸發全頁 reload。
 */
export function MessageTranslationButton({
  eligible,
  off,
  status,
  show,
  onFlip,
  onManual,
}: {
  eligible: boolean
  off: { reason: string } | null
  status: 'idle' | 'loading' | 'done' | 'error'
  show?: boolean
  onFlip: () => void
  onManual: () => void
}) {
  const t = useT()
  if (!eligible) return null
  const onClick = off
    ? () => useStore.getState().notify('info', t.tr(off.reason))
    : status === 'done' ? onFlip : status === 'loading' ? undefined : onManual
  const label = off ? t('翻譯（未啟用）') : status === 'loading' ? t('翻譯中…') : status === 'done' ? (show ? t('⇄ 原文') : t('⇄ 譯文')) : t('翻譯')
  return (
    <button type="button" className={`msg-translate${off ? ' off' : ''}`} disabled={status === 'loading'} onClick={onClick} title={off ? t.tr(off.reason) : t('用 Claude 翻成繁體中文')}>
      {label}
    </button>
  )
}

export function MessageTranslationContent({
  text,
  botId,
  cost,
  model,
}: {
  text: string
  botId?: string | null
  cost: number | null
  model: string
}) {
  const t = useT()
  return (
    <div className="msg-translation" lang="zh-Hant">
      <SafeMarkdown text={text} botId={botId} />
      <div className="msg-translation-cost">{costLine(t, { cost, model })}</div>
    </div>
  )
}
