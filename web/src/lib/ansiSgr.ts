
export type AnsiStyle = {
  fg?: string
  bg?: string
  bold?: boolean
  dim?: boolean
  italic?: boolean
  underline?: boolean
  inverse?: boolean
  strike?: boolean
}

export type AnsiSegment = {
  text: string
  style: AnsiStyle
  href?: string
}

const ESC = '\x1b'
const CSI_8BIT = '\x9b'
const CUBE_LEVELS = [0, 95, 135, 175, 215, 255]

function hex(r: number, g: number, b: number): string {
  return '#' + [r, g, b].map((v) => v.toString(16).padStart(2, '0')).join('')
}

const STYLE_KEYS = ['fg', 'bg', 'bold', 'dim', 'italic', 'underline', 'inverse', 'strike'] as const

function sameStyle(a: AnsiStyle, b: AnsiStyle): boolean {
  return STYLE_KEYS.every((k) => a[k] === b[k])
}

const clamp255 = (v: number) => (Number.isFinite(v) ? Math.min(255, Math.max(0, Math.trunc(v))) : 0)

function color256(n: number): string | undefined {
  if (!Number.isInteger(n) || n < 0 || n > 255) return undefined
  if (n < 16) return `ansi:${n}`
  if (n < 232) {
    const c = n - 16
    return hex(CUBE_LEVELS[Math.floor(c / 36)], CUBE_LEVELS[Math.floor((c % 36) / 6)], CUBE_LEVELS[c % 6])
  }
  const v = 8 + (n - 232) * 10
  return hex(v, v, v)
}

export function parseAnsi(input: string): AnsiSegment[] {
  const out: AnsiSegment[] = []
  const style: AnsiStyle = {}
  let href: string | undefined
  let text = ''

  const cut = () => {
    if (!text) return
    const last = out[out.length - 1]
    if (last && last.href === href && sameStyle(last.style, style)) last.text += text
    else {
      const seg: AnsiSegment = { text, style: { ...style } }
      if (href) seg.href = href
      out.push(seg)
    }
    text = ''
  }

  const setColor = (which: 'fg' | 'bg', c: string | undefined) => {
    if (c === undefined) delete style[which]
    else style[which] = c
  }

  const applySgr = (params: string) => {
    cut()
    const parts = params === '' ? [''] : params.split(';')
    for (let k = 0; k < parts.length; k++) {
      const part = parts[k]
      if (part.includes(':')) {
        const sub = part.split(':')
        const head = Number(sub[0])
        if (head === 38 || head === 48) {
          const which = head === 38 ? 'fg' : 'bg'
          if (sub[1] === '5') setColor(which, color256(Number(sub[2])))
          else if (sub[1] === '2' && sub.length >= 5) {
            const [r, g, b] = sub.slice(-3).map(Number)
            setColor(which, hex(clamp255(r), clamp255(g), clamp255(b)))
          }
        }
        continue
      }
      const n = part === '' ? 0 : Number(part)
      if (!Number.isInteger(n)) continue
      if (n === 0) {
        for (const key of Object.keys(style)) delete (style as Record<string, unknown>)[key]
      } else if (n === 1) style.bold = true
      else if (n === 2) style.dim = true
      else if (n === 3) style.italic = true
      else if (n === 4) style.underline = true
      else if (n === 7) style.inverse = true
      else if (n === 9) style.strike = true
      else if (n === 22) {
        delete style.bold
        delete style.dim
      } else if (n === 23) delete style.italic
      else if (n === 24) delete style.underline
      else if (n === 27) delete style.inverse
      else if (n === 29) delete style.strike
      else if (n >= 30 && n <= 37) style.fg = `ansi:${n - 30}`
      else if (n === 39) setColor('fg', undefined)
      else if (n >= 40 && n <= 47) style.bg = `ansi:${n - 40}`
      else if (n === 49) setColor('bg', undefined)
      else if (n >= 90 && n <= 97) style.fg = `ansi:${n - 90 + 8}`
      else if (n >= 100 && n <= 107) style.bg = `ansi:${n - 100 + 8}`
      else if (n === 38 || n === 48) {
        const which = n === 38 ? 'fg' : 'bg'
        if (parts[k + 1] === '5') {
          setColor(which, color256(Number(parts[k + 2])))
          k += 2
        } else if (parts[k + 1] === '2') {
          const [r, g, b] = parts.slice(k + 2, k + 5).map(Number)
          setColor(which, hex(clamp255(r), clamp255(g), clamp255(b)))
          k += 4
        }
      }
    }
  }

  const readCsi = (start: number): number => {
    let j = start
    while (j < input.length && input.charCodeAt(j) >= 0x20 && input.charCodeAt(j) <= 0x3f) j++
    if (j >= input.length) return input.length
    const body = input.slice(start, j)
    if (input[j] === 'm' && /^[0-9:;]*$/.test(body)) applySgr(body)
    return j + 1
  }

  const readString = (start: number, onBody?: (body: string) => void): number => {
    let j = start
    while (j < input.length) {
      const ch = input[j]
      if (ch === '\x07') {
        onBody?.(input.slice(start, j))
        return j + 1
      }
      if (ch === ESC && input[j + 1] === '\\') {
        onBody?.(input.slice(start, j))
        return j + 2
      }
      j++
    }
    return input.length
  }

  let i = 0
  while (i < input.length) {
    const ch = input[i]
    if (ch === ESC) {
      const next = input[i + 1]
      if (next === '[') {
        i = readCsi(i + 2)
      } else if (next === ']') {
        i = readString(i + 2, (body) => {
          if (body.startsWith('8;')) {
            const semi = body.indexOf(';', 2)
            const uri = semi >= 0 ? body.slice(semi + 1) : ''
            cut()
            href = uri || undefined
          }
        })
      } else if (next === 'P' || next === 'X' || next === '^' || next === '_') {
        i = readString(i + 2)
      } else if (next !== undefined && next >= ' ' && next <= '/') {
        let j = i + 1
        while (j < input.length && input[j] >= ' ' && input[j] <= '/') j++
        i = j + 1
      } else {
        i += 2
      }
      continue
    }
    if (ch === CSI_8BIT) {
      i = readCsi(i + 1)
      continue
    }
    if (ch === '\n' || ch === '\t') {
      text += ch
    } else {
      const code = ch.charCodeAt(0)
      if (code >= 0x20 && code !== 0x7f && !(code >= 0x80 && code <= 0x9f)) text += ch
    }
    i++
  }
  cut()
  return out
}

export function stripAnsi(text: string): string {
  return parseAnsi(text).map((s) => s.text).join('')
}
