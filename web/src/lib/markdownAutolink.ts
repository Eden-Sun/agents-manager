const STOP = new Set('）」』、。，；：！？》〉】〕“”‘’'.split(''))
const URL_IN_TEXT = /(?:https?:\/\/|www\.)[^\s<>"'`、。，；：！？》〉】〕“”‘’）」』]+/giu
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

const SPLIT_AT = new Set([...STOP, '"', "'", '`', '<', '>'])

function firstStop(s: string): number {
  for (let i = 0; i < s.length; i++) if (SPLIT_AT.has(s[i])) return i
  return -1
}

function textNode(value: string): MdNode {
  return { type: 'text', value }
}

function linkNode(text: string, target: string): MdNode {
  return { type: 'link', url: target, children: [textNode(text)] }
}

function linkifyText(text: string): MdNode[] {
  const out: MdNode[] = []
  const re = new RegExp(URL_IN_TEXT.source, URL_IN_TEXT.flags)
  let last = 0
  let m: RegExpExecArray | null
  while ((m = re.exec(text)) !== null) {
    const start = m.index
    const url = m[0].slice(0, urlEnd(m[0]))
    if (!URL_START.test(url) || (start > 0 && WORD_CHAR.test(text[start - 1]))) {
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

function isExplicitLink(node: MdNode, src: string | null): boolean {
  const off = node.position?.start.offset
  return src !== null && typeof off === 'number' && src.charAt(off) === '['
}

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

export function remarkTrimAutolinkPunct() {
  return (tree: MdNode, file?: { value?: unknown }) => {
    const src = typeof file?.value === 'string' ? file.value : null
    walk(tree, src)
  }
}
