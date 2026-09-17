import { defaultUrlTransform, type UrlTransform } from 'react-markdown'

/**
 * react-markdown 預設只放行 `http(s)`／`mailto`…，`![](file:///Users/…/shot.png)` 到元件手上是空字串，
 * 整張圖連路徑文字都不出現（review3 c4 L3）。只對 `<img src>` 放行 `file:`——交給 MarkdownImage 經 daemon
 * 從專案目錄讀；連結（`href`）照舊用預設規則。
 */
export const markdownUrlTransform: UrlTransform = (url, key, node) =>
  key === 'src' && node.tagName === 'img' && /^file:/i.test(url) ? url : defaultUrlTransform(url)

/** Markdown 渲染把圖片網址編過（`docs/截圖.png` → `docs/%E6%88%AA%E5%9C%96.png`）：讀不到時寫給人看的路徑要解回來。 */
export function readableImagePath(src: string): string {
  try {
    return decodeURI(src)
  } catch {
    return src
  }
}
