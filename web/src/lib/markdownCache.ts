import Markdown, { type UrlTransform } from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { remarkTrimAutolinkPunct } from './markdownAutolink'
import type { ReactNode } from 'react'
import { markdownComponents } from './markdownComponents'
import { markdownUrlTransform } from './markdownUrl'

export const MARKDOWN_CACHE_MAX_ENTRIES = 300
export const MARKDOWN_CACHE_MAX_CHARS = 1_000_000
export const MARKDOWN_CACHE_MAX_ONE = 50_000

interface Entry {
  botId: string
  components: ReturnType<typeof markdownComponents>
  linkComponent: ReturnType<typeof markdownComponents>['a']
  imageComponent: ReturnType<typeof markdownComponents>['img']
  urlTransform: UrlTransform
  node: ReactNode
}

const cache = new Map<string, Entry>()
let chars = 0
let parses = 0

export interface MarkdownRendererDependencies {
  components: ReturnType<typeof markdownComponents>
  urlTransform: UrlTransform
}

export function renderMarkdown(
  botId: string | null | undefined,
  content: string,
  dependencies?: MarkdownRendererDependencies,
): ReactNode {
  const bot = botId ?? ''
  const components = dependencies?.components ?? markdownComponents(botId)
  const urlTransform = dependencies?.urlTransform ?? markdownUrlTransform
  const hit = cache.get(content)
  if (
    hit &&
    hit.botId === bot &&
    hit.components === components &&
    hit.linkComponent === components.a &&
    hit.imageComponent === components.img &&
    hit.urlTransform === urlTransform
  ) {
    cache.delete(content)
    cache.set(content, hit)
    return hit.node
  }
  if (hit) {
    cache.delete(content)
    chars -= content.length
  }
  parses++
  const node = Markdown({ remarkPlugins: [remarkGfm, remarkTrimAutolinkPunct], components, urlTransform, children: content })
  if (content.length <= MARKDOWN_CACHE_MAX_ONE) {
    cache.set(content, { botId: bot, components, linkComponent: components.a, imageComponent: components.img, urlTransform, node })
    chars += content.length
    while ((cache.size > MARKDOWN_CACHE_MAX_ENTRIES || chars > MARKDOWN_CACHE_MAX_CHARS) && cache.size > 1) {
      const oldest = cache.keys().next().value as string
      chars -= oldest.length
      cache.delete(oldest)
    }
  }
  return node
}

export function discardMarkdown(botId: string | null | undefined, content: string): void {
  const hit = cache.get(content)
  if (!hit || hit.botId !== (botId ?? '')) return
  cache.delete(content)
  chars -= content.length
}

export function clearMarkdownCache(): void {
  cache.clear()
  chars = 0
  parses = 0
}

export function markdownCacheStats(): { size: number; chars: number; parses: number } {
  return { size: cache.size, chars, parses }
}
