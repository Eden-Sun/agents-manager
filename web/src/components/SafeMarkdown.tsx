import { Component, Fragment, useState, type ReactNode } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { discardMarkdown, renderMarkdown } from '../lib/markdownCache'
import { markdownComponents } from '../lib/markdownComponents'
import { markdownUrlTransform } from '../lib/markdownUrl'
import { markdownTooDeep, markdownTooLong } from '../lib/markdownGuard'

/**
 * 轉換丟例外（巢狀太深爆 stack）時退回純文字：整個 app 沒有 error boundary，沒接住的話
 * 一則 `> > > …` 的訊息會讓整個畫面白掉，而且訊息在歷史裡，重新整理還是白。
 */
export class MarkdownBoundary extends Component<{ plain: ReactNode; children: ReactNode; onError?: () => void }, { failed: boolean }> {
  state = { failed: false }
  static getDerivedStateFromError() {
    return { failed: true }
  }
  componentDidCatch() {
    this.props.onError?.()
  }
  render() {
    return this.state.failed ? this.props.plain : this.props.children
  }
}

/** bot 輸出的 Markdown（不可信）：不跑原始 HTML、危險協定被清掉、連結新分頁、太長或太深都有退路。 */
/** `cache=false`：串流中的草稿每幀都是新字串，放進快取只會擠掉真正的訊息。手動展開的超長／太深內容也不快取。 */
export function SafeMarkdown({ text, botId, cache = true }: { text: string; botId?: string | null; cache?: boolean }) {
  const [forced, setForced] = useState(false)
  const plain = (note: ReactNode) => (
    <>
      <pre className="md-plain">{text}</pre>
      {note}
    </>
  )
  const long = markdownTooLong(text)
  const deep = !long && markdownTooDeep(text)
  const shouldCache = cache && !long && !deep
  if ((long || deep) && !forced) {
    return plain(
      <p className="md-plain-note">
        {long ? `這則訊息很長（${text.length.toLocaleString()} 字）` : '這則訊息的巢狀層數很深'}，為了不讓畫面卡住先用純文字顯示。{' '}
        <button type="button" className="btn" onClick={() => setForced(true)}>
          以 Markdown 顯示
        </button>
      </p>,
    )
  }
  return (
    <Fragment key={botId ?? ''}>
      <MarkdownBoundary
        key={text}
        plain={plain(<p className="md-plain-note">這則訊息的格式太複雜，無法轉成 Markdown，以純文字顯示。</p>)}
        onError={shouldCache ? () => discardMarkdown(botId, text) : undefined}
      >
        <CachedMarkdown text={text} botId={botId} cache={shouldCache} />
      </MarkdownBoundary>
    </Fragment>
  )
}

/** 走快取（重掛不重新 parse）；包成元件放在 boundary 裡面，轉換丟例外才接得住（失敗的不會進快取）。 */
function CachedMarkdown({ text, botId, cache }: { text: string; botId?: string | null; cache: boolean }) {
  if (!cache)
    return (
      <Markdown remarkPlugins={[remarkGfm]} components={markdownComponents(botId)} urlTransform={markdownUrlTransform}>
        {text}
      </Markdown>
    )
  return <>{renderMarkdown(botId, text)}</>
}
