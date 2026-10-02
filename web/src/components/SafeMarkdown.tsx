import { Component, useState, type ReactNode } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { markdownComponents } from '../lib/markdownComponents'
import { markdownTooDeep, markdownTooLong } from '../lib/markdownGuard'
import { markdownUrlTransform } from '../lib/markdownUrl'

/**
 * 轉換丟例外（巢狀太深爆 stack）時退回純文字：整個 app 沒有 error boundary，沒接住的話
 * 一則 `> > > …` 的訊息會讓整個畫面白掉，而且訊息在歷史裡，重新整理還是白。
 */
export class MarkdownBoundary extends Component<{ plain: ReactNode; children: ReactNode }, { failed: boolean }> {
  state = { failed: false }
  static getDerivedStateFromError() {
    return { failed: true }
  }
  render() {
    return this.state.failed ? this.props.plain : this.props.children
  }
}

/** bot 輸出的 Markdown（不可信）：不跑原始 HTML、危險協定被清掉、連結新分頁、太長或太深都有退路。 */
export function SafeMarkdown({ text, botId }: { text: string; botId?: string | null }) {
  const [forced, setForced] = useState(false)
  const plain = (note: ReactNode) => (
    <>
      <pre className="md-plain">{text}</pre>
      {note}
    </>
  )
  const long = markdownTooLong(text)
  if ((long || markdownTooDeep(text)) && !forced) {
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
    <MarkdownBoundary key={text} plain={plain(<p className="md-plain-note">這則訊息的格式太複雜，無法轉成 Markdown，以純文字顯示。</p>)}>
      <Markdown remarkPlugins={[remarkGfm]} components={markdownComponents(botId)} urlTransform={markdownUrlTransform}>
        {text}
      </Markdown>
    </MarkdownBoundary>
  )
}
