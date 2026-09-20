import type { ComponentProps } from 'react'
import { MarkdownImage } from '../components/MarkdownImage'

type MarkdownComponents = { img: (props: ComponentProps<'img'>) => React.JSX.Element }

/**
 * 同一顆 bot 永遠回同一組：`img` 是元件型別，每次 render 換一個新函式，React 就會把訊息裡的圖
 * 卸掉重掛（本機圖片還要重抓一次 daemon blob）。訊息清單重載（resync、送失敗重讀）時每則訊息都是新物件，
 * 會讓整段歷史的圖一起閃回「載入中」。
 */
const cache = new Map<string, MarkdownComponents>()

/** 給 `<Markdown components={…}>` 用。 */
export function markdownComponents(botId: string | null | undefined): MarkdownComponents {
  const key = botId ?? ''
  let c = cache.get(key)
  if (!c) {
    c = {
      img: (props: ComponentProps<'img'>) => <MarkdownImage botId={botId} src={typeof props.src === 'string' ? props.src : undefined} alt={props.alt} />,
    }
    cache.set(key, c)
  }
  return c
}
