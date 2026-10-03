import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'

/**
 * 分享頁的 bot 回覆：GFM，但不吃 HTML、不載圖片（外部圖片會把讀者的 IP 洩給第三方），連結一律開新分頁且不帶 referrer。
 * 跟主 UI 的 SafeMarkdown 分開：那一支會打主 API 讀附件，分享頁不能碰。
 */
export default function ShareMarkdown({ text }: { text: string }) {
  return (
    <div className="sh-md">
      <Markdown
        remarkPlugins={[remarkGfm]}
        skipHtml
        disallowedElements={['img']}
        unwrapDisallowed
        components={{
          a: ({ href, children }) => (
            <a href={href} target="_blank" rel="noopener noreferrer nofollow">
              {children}
            </a>
          ),
        }}
      >
        {text}
      </Markdown>
    </div>
  )
}
