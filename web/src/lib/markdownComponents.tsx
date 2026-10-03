import type { ComponentProps } from 'react'
import { MarkdownImage } from '../components/MarkdownImage'
import { safeHttpUrl } from './safeUrl'

type MarkdownComponents = {
  img: (props: ComponentProps<'img'>) => React.JSX.Element
  a: (props: ComponentProps<'a'>) => React.JSX.Element
}

/**
 * 同一顆 bot 永遠回同一組：`img` 是元件型別，每次 render 換一個新函式，React 就會把訊息裡的圖
 * 卸掉重掛（本機圖片還要重抓一次 daemon blob）。訊息清單重載（resync、送失敗重讀）時每則訊息都是新物件，
 * 會讓整段歷史的圖一起閃回「載入中」。
 */
const MARKDOWN_COMPONENTS_CACHE_LIMIT = 300
const cache = new Map<string, MarkdownComponents>()

/** 給 `<Markdown components={…}>` 用。 */
export function markdownComponents(botId: string | null | undefined): MarkdownComponents {
  const key = botId ?? ''
  let c = cache.get(key)
  if (c) {
    // Recent bots keep their component type across remounts; deleted or long-unused ids eventually leave the Map.
    cache.delete(key)
    cache.set(key, c)
    return c
  }

  c = {
    // bot 給的連結：一律新分頁＋noopener noreferrer，不然一按就整個 app 換頁（連同記憶體裡的 token），
    // 也不能讓對方網站拿到 opener／referrer。被 urlTransform 清掉網址的（`javascript:` 等）不留一個點了沒反應的 `<a>`。
    a: ({ href, children }: ComponentProps<'a'>) => {
      // urlTransform 仍放行相對路徑與 //host。相對的 /api/session 在這個 origin 開新分頁會拿到 UI token。
      const safe = safeHttpUrl(typeof href === 'string' ? href : undefined)
      return safe ? (
        <a href={safe} target="_blank" rel="noopener noreferrer">
          {children}
        </a>
      ) : (
        <span>{children}</span>
      )
    },
    img: (props: ComponentProps<'img'>) => <MarkdownImage botId={botId} src={typeof props.src === 'string' ? props.src : undefined} alt={props.alt} />,
  }
  cache.set(key, c)
  if (cache.size > MARKDOWN_COMPONENTS_CACHE_LIMIT) {
    const oldest = cache.keys().next().value
    if (oldest !== undefined) cache.delete(oldest)
  }
  return c
}
