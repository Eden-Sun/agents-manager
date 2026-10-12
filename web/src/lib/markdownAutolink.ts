/**
 * 訊息泡泡的自動連結規則（remark-gfm 的 autolink literal）：網址後面緊接的全形標點不算進網址，
 * 跟終端的網址規則一致（`termPieces`，#1533）。remark-gfm 只認 ASCII 標點，會把「…/login，然後貼上」整句包成連結。
 *
 * 只動自動連結（連結文字就是網址本身，或 `www.` 那種去掉 `http://` 的寫法）；寫成 `[文字](網址)` 的連結照原樣。
 * 在第一個全形標點切開後，後面那一段是一般文字：同一段裡後面再出現的網址（`a、https://b`）要照同一規則重新認出來，
 * 否則 remark 已經把它們併進前一個連結，切開後就變成不能點的文字。
 */
const STOP = new Set('）」』、。，；：！？》〉】〕“”‘’'.split(''))
const URL_IN_TEXT = /https?:\/\/[^\s<>"'`]+/g

interface MdNode {
  type: string
  value?: string
  url?: string
  children?: MdNode[]
}

function cutAtStop(raw: string): string {
  for (let i = 0; i < raw.length; i++) if (STOP.has(raw[i])) return raw.slice(0, i)
  return raw
}

function textNode(value: string): MdNode {
  return { type: 'text', value }
}

function linkNode(url: string): MdNode {
  return { type: 'link', url, children: [textNode(url)] }
}

/** 一般文字（已經不是 remark 認出來的連結）：裡面的網址照同一規則切。 */
function linkifyText(text: string): MdNode[] {
  const out: MdNode[] = []
  let last = 0
  for (const m of text.matchAll(URL_IN_TEXT)) {
    const start = m.index ?? 0
    // 句點、逗號等 ASCII 句尾標點不算網址（同 remark 的規則）。
    const url = cutAtStop(m[0]).replace(/[.,;:!?]+$/, '')
    if (!/^https?:\/\/./.test(url)) continue
    if (start > last) out.push(textNode(text.slice(last, start)))
    out.push(linkNode(url))
    last = start + url.length
  }
  if (last < text.length) out.push(textNode(text.slice(last)))
  return out
}

/** 自動連結在第一個全形標點前面切開：前半還是連結，後半重新認網址、其餘變成一般文字。 */
function splitAutolink(node: MdNode): MdNode[] | null {
  const kids = node.children
  if (!node.url || kids?.length !== 1 || kids[0].type !== 'text' || kids[0].value === undefined) return null
  const text = kids[0].value
  let at = -1
  for (let i = 0; i < text.length; i++) {
    if (STOP.has(text[i])) {
      at = i
      break
    }
  }
  if (at <= 0) return null
  // 全形標點前面的 ASCII 句點、逗號也不算網址（`…/x.。` 與 termPieces 同一個結果）。
  const asciiTail = /[.,;:!?]+$/.exec(text.slice(0, at))?.[0] ?? ''
  const head = text.slice(0, at - asciiTail.length)
  const tail = asciiTail + text.slice(at)
  // 只認自動連結的形狀：連結文字＝網址，或網址＝`http://`＋連結文字（remark 對 `www.` 的寫法）。
  if (node.url !== text && node.url !== `http://${text}`) return null
  const url = node.url.slice(0, node.url.length - tail.length)
  return [{ ...node, url, children: [textNode(head)] }, ...linkifyText(tail)]
}

function walk(node: MdNode): void {
  if (!node.children) return
  const out: MdNode[] = []
  for (const child of node.children) {
    const parts = child.type === 'link' ? splitAutolink(child) : null
    if (parts) {
      out.push(...parts)
    } else {
      walk(child)
      out.push(child)
    }
  }
  node.children = out
}

/** remark 外掛：放在 `remarkGfm` 後面（`remarkPlugins={[remarkGfm, remarkTrimAutolinkPunct]}`）。 */
export function remarkTrimAutolinkPunct() {
  return (tree: MdNode) => {
    walk(tree)
  }
}
