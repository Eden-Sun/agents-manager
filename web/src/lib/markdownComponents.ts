import { createElement, type ComponentProps } from 'react'
import { MarkdownImage } from '../components/MarkdownImage'
import { safeHttpUrl } from './safeUrl'

type MarkdownComponents = {
  img: (props: ComponentProps<'img'>) => React.JSX.Element
  a: (props: ComponentProps<'a'>) => React.JSX.Element
}

const MARKDOWN_COMPONENTS_CACHE_LIMIT = 300
const cache = new Map<string, MarkdownComponents>()

export function markdownComponents(botId: string | null | undefined): MarkdownComponents {
  const key = botId ?? ''
  let c = cache.get(key)
  if (c) {
    cache.delete(key)
    cache.set(key, c)
    return c
  }

  c = {
    a: ({ href, children }: ComponentProps<'a'>) => {
      const safe = safeHttpUrl(typeof href === 'string' ? href : undefined)
      return safe
        ? createElement('a', { href: safe, target: '_blank', rel: 'noopener noreferrer' }, children)
        : createElement('span', null, children)
    },
    img: (props: ComponentProps<'img'>) =>
      createElement(MarkdownImage, {
        botId,
        src: typeof props.src === 'string' ? props.src : undefined,
        alt: props.alt,
      }),
  }
  cache.set(key, c)
  if (cache.size > MARKDOWN_COMPONENTS_CACHE_LIMIT) {
    const oldest = cache.keys().next().value
    if (oldest !== undefined) cache.delete(oldest)
  }
  return c
}
