import { Component, type ReactNode } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { remarkTrimAutolinkPunct } from '../lib/markdownAutolink'
import { markdownTooDeep, markdownTooLong } from '../lib/markdownGuard'
import { shareSafeHref } from './shareModel'
import { useT } from './shareI18n'

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
      remarkPlugins={[remarkGfm, remarkTrimAutolinkPunct]}
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
  const t = useT()
  const plain = (
    <pre className="sh-plain">{text === '__share_md_boom__' ? t('這則訊息的格式太複雜，無法轉成 Markdown。') : text}</pre>
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
