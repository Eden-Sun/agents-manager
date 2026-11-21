import { Component, Fragment, lazy, Suspense, useState, type ReactNode } from 'react'
import { useT } from '../lib/i18n'
import { markdownTooDeep, markdownTooLong } from '../lib/markdownGuard'

const CachedMarkdown = lazy(() => import('./SafeMarkdownContent'))

export class MarkdownBoundary extends Component<
  { plain: ReactNode; children: ReactNode; onError?: () => void; resetKey?: unknown },
  { failed: boolean }
> {
  state = { failed: false }
  static getDerivedStateFromError() {
    return { failed: true }
  }
  componentDidUpdate(prev: Readonly<{ resetKey?: unknown }>) {
    if (this.state.failed && prev.resetKey !== this.props.resetKey) this.setState({ failed: false })
  }
  componentDidCatch() {
    this.props.onError?.()
  }
  render() {
    return this.state.failed ? this.props.plain : this.props.children
  }
}

export function MarkdownSuspense({ text, children }: { text: string; children: ReactNode }) {
  return <Suspense fallback={<pre className="md-plain">{text}</pre>}>{children}</Suspense>
}

export function SafeMarkdown({ text, botId, cache = true }: { text: string; botId?: string | null; cache?: boolean }) {
  const t = useT()
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
        {long ? t('這則訊息很長（{count} 字）', { count: text.length.toLocaleString() }) : t('這則訊息的巢狀層數很深')}
        {t('，為了不讓畫面卡住先用純文字顯示。')}{' '}
        <button type="button" className="btn" onClick={() => setForced(true)}>
          {t('以 Markdown 顯示')}
        </button>
      </p>,
    )
  }
  return (
    <Fragment key={botId ?? ''}>
      <MarkdownBoundary
        resetKey={text}
        plain={plain(<p className="md-plain-note">{t('這則訊息的格式太複雜，無法轉成 Markdown，以純文字顯示。')}</p>)}
        onError={shouldCache ? () => {
          void import('../lib/markdownCache')
            .then(({ discardMarkdown }) => discardMarkdown(botId, text))
            .catch(() => {})
        } : undefined}
      >
        <MarkdownSuspense text={text}>
          <CachedMarkdown text={text} botId={botId} cache={shouldCache} />
        </MarkdownSuspense>
      </MarkdownBoundary>
    </Fragment>
  )
}
