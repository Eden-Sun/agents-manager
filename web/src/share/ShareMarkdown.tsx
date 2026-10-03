import { Component, type ReactNode } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { markdownTooDeep, markdownTooLong } from '../lib/markdownGuard'
import { shareSafeHref } from './shareModel'

/**
 * 分享頁的 bot 回覆：GFM，但不吃 HTML、不載圖片，連結一律開新分頁且不帶 referrer。
 * 複雜度上限跟主 UI 的 SafeMarkdown 同一套純函式。公開頁不提供「仍用 Markdown 渲染」。
 * 不 import 主 UI 元件（那些會帶管理 API）。
 */
class ShareMarkdownBoundary extends Component<{ plain: ReactNode; children: ReactNode }, { failed: boolean }> {
  state = { failed: false }
  static getDerivedStateFromError() {
    return { failed: true }
  }
  render() {
    return this.state.failed ? this.props.plain : this.props.children
  }
}

function MarkdownBody({ text }: { text: string }) {
  if (text === '__share_md_boom__') throw new Error('share markdown boom')
  return (
    <Markdown
      remarkPlugins={[remarkGfm]}
      skipHtml
      disallowedElements={['img']}
      unwrapDisallowed
      components={{
        a: ({ href, children }) => {
          const safe = shareSafeHref(href)
          if (!safe) return <span>{children}</span>
          return (
            <a href={safe} target="_blank" rel="noopener noreferrer nofollow">
              {children}
            </a>
          )
        },
      }}
    >
      {text}
    </Markdown>
  )
}

export default function ShareMarkdown({ text }: { text: string }) {
  const plain = (
    <pre className="sh-plain">{text === '__share_md_boom__' ? '這則訊息的格式太複雜，無法轉成 Markdown。' : text}</pre>
  )
  if (markdownTooLong(text) || markdownTooDeep(text)) return <div className="sh-md">{plain}</div>
  return (
    <div className="sh-md">
      <ShareMarkdownBoundary plain={plain}>
        <MarkdownBody text={text} />
      </ShareMarkdownBoundary>
    </div>
  )
}
