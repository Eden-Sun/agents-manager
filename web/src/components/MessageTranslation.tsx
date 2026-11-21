import { useState } from 'react'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { SafeMarkdown } from './SafeMarkdown'
import { MessageTranslateRecordSheet } from './TranslateRetroRecords'
import { isLocalTranslation, translationModelName, usd } from './messageTranslationConstants'
import type { RejectDetail } from '../lib/messageTranslationStore'

function costLine(t: ReturnType<typeof useT>, st: { cost: number | null; model: string }): string {
  const model = translationModelName(st.model)
  if (isLocalTranslation(st.model) || st.cost === null) return t('(by {model})', { model })
  return t('(by {model} {cost})', { model, cost: usd(st.cost) })
}

function RecordTrigger({ messageId }: { messageId: string }) {
  const t = useT()
  const [open, setOpen] = useState(false)
  return (
    <>
      <button
        type="button"
        className="msg-translate msg-translate-retro"
        data-k="translate-retro"
        onClick={(e) => {
          e.stopPropagation()
          setOpen(true)
        }}
        title={t('查看這則譯文的翻譯紀錄')}
      >
        {t('複盤')}
      </button>
      {open ? <MessageTranslateRecordSheet messageId={messageId} open onClose={() => setOpen(false)} /> : null}
    </>
  )
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
  ready,
  spin,
  onFlip,
  onManual,
  rejectedReason,
  rejectDetail,
  onShowAnyway,
  onReport,
  onRedo,
  redoing,
  messageId,
}: {
  eligible: boolean
  off: { reason: string } | null
  status: 'idle' | 'loading' | 'done' | 'error' | 'rejected'
  show?: boolean
  ready?: boolean
  spin?: 'forward' | 'backward' | null
  onFlip: (e?: React.MouseEvent) => void
  onManual: () => void
  onRedo?: () => void
  redoing?: boolean
  rejectedReason?: string
  rejectDetail?: RejectDetail
  onShowAnyway?: () => void
  onReport?: () => void
  messageId?: string
}) {
  const t = useT()
  if (!eligible) return null
  const isReady = status === 'done' && Boolean(ready) && !show

  const handleFlip = (e: React.MouseEvent) => {
    // 錨定：原本貼底就保持貼底；手動點擊時以被點訊息的標頭為錨保持位置
    const btn = e.currentTarget as HTMLElement
    const box = (btn.closest('.msg-list') ?? btn.closest('[data-list]')) as HTMLElement | null
    const msgEl = btn.closest('[data-msg-id]') as HTMLElement | null
    const head = (msgEl?.querySelector('.msg-meta') as HTMLElement | null) ?? msgEl
    const wasBottom = box ? box.scrollHeight - box.scrollTop - box.clientHeight < 30 : false
    const headTop = box && head ? head.getBoundingClientRect().top - box.getBoundingClientRect().top : 0

    onFlip(e)

    if (box) {
      const adjust = () => {
        if (wasBottom) {
          box.scrollTop = box.scrollHeight
        } else if (head) {
          const now = head.getBoundingClientRect().top - box.getBoundingClientRect().top
          const diff = now - headTop
          if (Math.abs(diff) > 0.5) {
            box.scrollTop += diff
          }
        }
      }
      adjust()
      if (typeof requestAnimationFrame !== 'undefined') {
        requestAnimationFrame(adjust)
      }
    }
  }

  const onClick = off
    ? () => useStore.getState().notify('info', t.tr(off.reason))
    : status === 'done'
      ? handleFlip
      : status === 'loading'
        ? undefined
        : onManual

  const label = off
    ? t('翻譯（未啟用）')
    : status === 'loading'
      ? t('翻譯中…')
      : isReady
        ? t('譯文已就緒')
        : status === 'done'
          ? (show ? t('⇄ 原文') : t('⇄ 譯文'))
          : t('翻譯')

  const ariaLabel = isReady || !show ? t('顯示譯文') : t('顯示原文')
  const title = off ? t.tr(off.reason) : t('用 Claude 翻成繁體中文')

  let content: React.ReactNode = label
  if (status === 'done' && !isReady && typeof label === 'string' && label.startsWith('⇄ ')) {
    content = (
      <>
        <span className={`msg-translate-ico${spin ? ` spin-${spin}` : ''}`} aria-hidden="true">
          ⇄
        </span>{' '}
        <span className="msg-translate-lbl">{label.slice(2)}</span>
      </>
    )
  }

  const button = (
    <button
      type="button"
      className={`msg-translate${off ? ' off' : ''}${isReady ? ' ready' : ''}`}
      disabled={status === 'loading'}
      onClick={onClick}
      title={title}
      aria-label={ariaLabel}
    >
      {content}
    </button>
  )
  const redoButton =
    status === 'done' && onRedo && !off ? (
      <button type="button" className="msg-translate msg-translate-redo" data-k="translate-redo" disabled={redoing} onClick={onRedo} title={t('用 Claude 重新翻譯這則（用升級模型，成功覆寫譯文）')} aria-label={t('重新翻譯這則')}>
        <span aria-hidden="true">↻</span>{' '}
        <span className="msg-translate-lbl">{redoing ? t('重新翻譯中…') : t('重新翻譯')}</span>
      </button>
    ) : null
  if (off || status !== 'rejected') {
    return redoButton ? (
      <>
        {button}
        {redoButton}
      </>
    ) : button
  }
  return (
    <>
      {button}
      <span className="msg-translate-hint" data-k="translate-rejected">
        {rejectedReason === 'japanese_output'
          ? t('譯文含日文，品質檢查未通過')
          : rejectedReason === 'untranslated'
            ? t('譯文跟原文幾乎一樣（沒翻成中文），品質檢查未通過')
            : rejectedReason === 'partially_untranslated'
              ? t('譯文仍有英文段落沒翻，品質檢查未通過')
              : t('這則沒有自動翻譯（品質檢查未通過），可以手動再試')}
        {onShowAnyway ? <> · <button type="button" className="msg-translate" data-k="translate-show-anyway" onClick={onShowAnyway}>{t('仍然顯示譯文')}</button></> : null}
        {onReport ? <> · <button type="button" className="msg-translate" data-k="translate-report" onClick={onReport}>{t('回報給管理者')}</button></> : null}
        {messageId ? <> · <RecordTrigger messageId={messageId} /></> : null}
      </span>
      {rejectDetail ? <RejectReasonDetails t={t} d={rejectDetail} /> : null}
    </>
  )
}

function RejectReasonDetails({ t, d }: { t: ReturnType<typeof useT>; d: RejectDetail }) {
  return (
    <details className="msg-translate-detail" data-k="translate-reject-detail">
      <summary>{t('原因')}</summary>
      <ul>
        <li>{t('檢查')}：{d.check}</li>
        <li>{t('原文字數')}：{t('拉丁 {latin}／CJK {cjk}', { latin: d.src_latin, cjk: d.src_cjk })}</li>
        <li>{t('譯文字數')}：{t('拉丁 {latin}／CJK {cjk}', { latin: d.reply_latin, cjk: d.reply_cjk })}</li>
        <li>{t('譯文與原文相同')}：{d.identical ? t('是') : t('否')}</li>
        <li>{t('嘗試')}：{t('{n} 次', { n: d.attempts })}（{d.models.join(' → ')}）</li>
      </ul>
    </details>
  )
}

export function MessageTranslationContent({
  messageId,
  text,
  botId,
  cost,
  model,
  lowConfidence,
  jevProb,
}: {
  messageId?: string
  text: string
  botId?: string | null
  cost: number | null
  model: string
  lowConfidence?: boolean
  jevProb?: number | null
}) {
  const t = useT()
  return (
    <div className="msg-translation" lang="zh-Hant">
      <SafeMarkdown text={text} botId={botId} />
      <div className="msg-translation-foot">
        <div className="msg-translation-cost">{costLine(t, { cost, model })}</div>
        {messageId ? <RecordTrigger messageId={messageId} /> : null}
      </div>
      {lowConfidence && jevProb != null ? (
        <div className="msg-translation-lowconf">{t('品質分數偏低（Jev {score}）', { score: jevProb.toFixed(2) })}</div>
      ) : null}
    </div>
  )
}
