/**
 * 已解析的 Markdown 快取：換 bot（或群組畫面）再換回來，清單整個重掛，每則 assistant 訊息的 Markdown
 * 又要重新 parse＋轉成 React 樹（一則帶 code block 的約 3–5 ms，200 則就是近一秒的主執行緒）。
 * `Bubble` 的 `memo` 只擋得住「同一次掛載裡」的重 render，擋不住重掛。
 *
 * 快取的是 `react-markdown` 的輸出（React 元素樹，不可變，可以安心掛在不同地方）。`Markdown` 是沒有 hook 的同步函式，
 * 直接呼叫它拿樹。鍵是文字本身（跟 store 裡的字串同一份，不另外複製）＋ bot id（圖片元件綁 bot）＋ renderer 身分。
 * 有上限（則數與總字數），超過就丟最久沒用的——這是記憶體上的取捨：長時間開著的分頁不能因此無限長。
 */
import Markdown, { type UrlTransform } from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { ReactNode } from 'react'
import { markdownComponents } from './markdownComponents'
import { markdownUrlTransform } from './markdownUrl'

export const MARKDOWN_CACHE_MAX_ENTRIES = 300
/** 快取住的原文總字數上限（樹的大小跟原文成正比，這個數字就是記憶體上限的代理）。 */
export const MARKDOWN_CACHE_MAX_CHARS = 1_000_000
/** 單則超過這麼長就不快取（巨大的工具輸出：留下它會擠掉幾百則一般訊息）。 */
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

/** @internal Override hooks let tests model a hot renderer/policy replacement in one page session. */
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
    // 最近用過：移到最後（Map 依插入順序，最前面就是最久沒用的）。
    cache.delete(content)
    cache.set(content, hit)
    return hit.node
  }
  // Same content with a different bot/renderer is a distinct tree. Discard the stale entry before
  // parsing so an exception cannot leave the old policy's node available for a later retry.
  if (hit) {
    cache.delete(content)
    chars -= content.length
  }
  parses++
  const node = Markdown({ remarkPlugins: [remarkGfm], components, urlTransform, children: content })
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

/** A boundary can remove a tree if a descendant component throws after Markdown produced it. */
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
