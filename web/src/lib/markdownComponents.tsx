import type { ComponentProps } from 'react'
import { MarkdownImage } from '../components/MarkdownImage'

/** 給 `<Markdown components={…}>` 用。 */
export function markdownComponents(botId: string | null | undefined) {
  return {
    img: (props: ComponentProps<'img'>) => <MarkdownImage botId={botId} src={typeof props.src === 'string' ? props.src : undefined} alt={props.alt} />,
  }
}
