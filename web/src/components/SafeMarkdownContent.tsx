import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { renderMarkdown } from '../lib/markdownCache'
import { markdownComponents } from '../lib/markdownComponents'
import { markdownUrlTransform } from '../lib/markdownUrl'

/** 走快取（重掛不重新 parse）；串流草稿直接 render，不佔一般訊息的快取。 */
export default function SafeMarkdownContent({ text, botId, cache }: { text: string; botId?: string | null; cache: boolean }) {
  if (!cache)
    return (
      <Markdown remarkPlugins={[remarkGfm]} components={markdownComponents(botId)} urlTransform={markdownUrlTransform}>
        {text}
      </Markdown>
    )
  return <>{renderMarkdown(botId, text)}</>
}
