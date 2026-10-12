/**
 * 訊息泡泡的自動連結規則（remark-gfm 的 autolink literal）：網址後面緊接的全形標點不算進網址，
 * 跟終端的網址規則一致（`termPieces`，#1533）。remark-gfm 只認 ASCII 標點，會把「…/login，然後貼上」整句包成連結。
 *
 * 只動自動連結（連結文字就是網址本身，或 `www.` 那種去掉 `http://` 的寫法）；寫成 `[文字](網址)` 的連結照原樣。
 * 在第一個全形標點切開後，後面那一段是一般文字：同一段裡後面再出現的網址（`a、https://b`、`a、www.b`）要照同一規則重新認出來，
 * 否則 remark 已經把它們併進前一個連結，切開後就變成不能點的文字。
 *
 * 句尾不算網址的：ASCII 句尾標點（`.,;:!?`），以及沒有配對的右括號／右方括號（`(https://…/a)。` 的網址是 `…/a`，
 * 同 termPieces 的 trimPunct）。
 */
const STOP = new Set('）」』、。，；：！？》〉】〕“”‘’'.split(''))
/** 一段文字裡的網址：不含空白、角括號、引號，也不含全形標點（否則一長串 `…，https://…` 會每次掃到行尾，變成平方時間）。 */
const URL_IN_TEXT = /(?:https?:\/\/|www\.)[^\s<>"'`、。，；：！？》〉】〕“”‘’）」』]+/giu
/** 網址前面不能緊接字母、數字（`xhttps://` 不是網址）。不用 lookbehind：舊版 iOS Safari 不認。 */
const WORD_CHAR = /[\p{L}\p{N}_]/u
const URL_START = /^(?:https?:\/\/|www\.)./i

interface MdNode {
  type: string
  value?: string
  url?: string
  children?: MdNode[]
  position?: { start: { offset?: number } }
}

function countOf(s: string, ch: string): number {
  let n = 0
  for (const c of s) if (c === ch) n++
  return n
}

/**
 * 網址的長度（不含句尾不算的部分）。從尾端往前剝，計數只算一次，整段線性。
 * 右括號只在「多過左括號」時才剝：成對的是網址自己的（`…/Foo_(bar)`）。
 */
function urlEnd(s: string): number {
  let end = s.length
  let open = countOf(s, '(')
  let close = countOf(s, ')')
  let sqOpen = countOf(s, '[')
  let sqClose = countOf(s, ']')
  while (end > 0) {
    const last = s[end - 1]
    if ('.,;:!?'.includes(last)) {
      end--
    } else if (last === ')' && close > open) {
      close--
      end--
    } else if (last === ']' && sqClose > sqOpen) {
      sqClose--
      end--
    } else {
      break
    }
  }
  return end
}

/** 切開的位置：全形標點，外加終端也不算網址的 ASCII 引號與角括號（`URL_IN_TEXT` 的字元類別同一份）。 */
const SPLIT_AT = new Set([...STOP, '"', "'", '`', '<', '>'])

function firstStop(s: string): number {
  for (let i = 0; i < s.length; i++) if (SPLIT_AT.has(s[i])) return i
  return -1
}

function textNode(value: string): MdNode {
  return { type: 'text', value }
}

/** `target` 是連結目標（`www.` 要補 `http://`）；連結文字就是看到的那段字。 */
function linkNode(text: string, target: string): MdNode {
  return { type: 'link', url: target, children: [textNode(text)] }
}

/** 一般文字（已經不是 remark 認出來的連結）：裡面的網址照同一規則切。 */
function linkifyText(text: string): MdNode[] {
  const out: MdNode[] = []
  const re = new RegExp(URL_IN_TEXT.source, URL_IN_TEXT.flags)
  let last = 0
  let m: RegExpExecArray | null
  while ((m = re.exec(text)) !== null) {
    const start = m.index
    const url = m[0].slice(0, urlEnd(m[0]))
    if (!URL_START.test(url) || (start > 0 && WORD_CHAR.test(text[start - 1]))) {
      // 不是網址（例如只有 `https://`）：從下一個字再找，後面可能還有真的網址。
      re.lastIndex = start + 1
      continue
    }
    if (start > last) out.push(textNode(text.slice(last, start)))
    out.push(linkNode(url, /^www\./i.test(url) ? `http://${url}` : url))
    last = start + url.length
    re.lastIndex = last
  }
  if (last < text.length) out.push(textNode(text.slice(last)))
  return out
}

/**
 * remark 認出的連結是不是「寫成 `[文字](網址)`」：原始碼在它的開頭是 `[`。
 * 文字與目標碰巧相同時 mdast 分不出來，只能看原始碼；讀不到原始碼就當成自動連結（舊行為）。
 */
function isExplicitLink(node: MdNode, src: string | null): boolean {
  const off = node.position?.start.offset
  return src !== null && typeof off === 'number' && src.charAt(off) === '['
}

/** 自動連結在第一個全形標點前面切開：前半還是連結，後半重新認網址、其餘變成一般文字。 */
function splitAutolink(node: MdNode, src: string | null): MdNode[] | null {
  const kids = node.children
  if (!node.url || kids?.length !== 1 || kids[0].type !== 'text' || kids[0].value === undefined) return null
  if (isExplicitLink(node, src)) return null
  const text = kids[0].value
  const stop = firstStop(text)
  if (stop <= 0) return null
  const head = text.slice(0, urlEnd(text.slice(0, stop)))
  if (!URL_START.test(head)) return null
  const tail = text.slice(head.length)
  // 只認自動連結的形狀：連結文字＝網址，或網址＝`http://`＋連結文字（remark 對 `www.` 的寫法）。
  if (node.url !== text && node.url !== `http://${text}`) return null
  const url = node.url.slice(0, node.url.length - tail.length)
  return [{ ...node, url, children: [textNode(head)] }, ...linkifyText(tail)]
}

function walk(node: MdNode, src: string | null): void {
  if (!node.children) return
  const out: MdNode[] = []
  for (const child of node.children) {
    const parts = child.type === 'link' ? splitAutolink(child, src) : null
    if (parts) {
      out.push(...parts)
    } else {
      walk(child, src)
      out.push(child)
    }
  }
  node.children = out
}

/** remark 外掛：放在 `remarkGfm` 後面（`remarkPlugins={[remarkGfm, remarkTrimAutolinkPunct]}`）。 */
export function remarkTrimAutolinkPunct() {
  return (tree: MdNode, file?: { value?: unknown }) => {
    const src = typeof file?.value === 'string' ? file.value : null
    walk(tree, src)
  }
}
