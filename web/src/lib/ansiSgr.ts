/**
 * 終端文字（含 ANSI 跳脫序列）→ 帶樣式的片段（純函式，沒有 JSX）。
 *
 * 只認 SGR（`ESC [ … m`）：重設、粗體、淡、斜、底線、反白、刪除線、16 色、256 色、truecolor。
 * OSC 8 超連結保留成 `href`；其他 OSC、游標移動、清除行、DCS／APC 等一律丟掉，`\r` 與其他控制字元也丟，
 * 所以原始的 ESC 不會出現在畫面上。`\n`、`\t` 原樣保留。
 *
 * 顏色：0–15 是 `ansi:N`（由 CSS 變數 `--ansi-N` 決定，才能跟著淺／深主題走）；256 色的 16–255 與 truecolor 直接換成 `#rrggbb`。
 */

export type AnsiStyle = {
  /** `ansi:0`–`ansi:15` 或 `#rrggbb`。 */
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
  /** OSC 8 超連結的目標；沒有超連結就沒有這個欄位。 */
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

/** 256 色調色盤的顏色：0–15 用主題變數，16–255 是固定的色塊與灰階。 */
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

/**
 * 解析一串終端文字。回傳的片段依序串起來就是 `stripAnsi(text)`；同樣樣式的相鄰字元會合併成一段。
 */
export function parseAnsi(input: string): AnsiSegment[] {
  const out: AnsiSegment[] = []
  const style: AnsiStyle = {}
  let href: string | undefined
  let text = ''

  // 把目前累積的文字收成一段（先收、再改樣式，所以段內的樣式是切換前的快照）。
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

  /** 套用一個 SGR 參數串（`ESC [ <params> m` 裡的 `<params>`）。 */
  const applySgr = (params: string) => {
    cut()
    const parts = params === '' ? [''] : params.split(';')
    for (let k = 0; k < parts.length; k++) {
      const part = parts[k]
      // 冒號子參數：`38:5:n`、`38:2::r:g:b`（也有 `38:2:cs:r:g:b`）。
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

  /**
   * 從 `start`（CSI 的參數起點）讀到最終位元組；回傳下一個要讀的位置。
   * 參數、中間位元組（0x20–0x3F）整段收掉；只有內容純粹是數字、冒號、分號且結尾是 `m` 才當 SGR 套用，其他一律丟掉。
   */
  const readCsi = (start: number): number => {
    let j = start
    while (j < input.length && input.charCodeAt(j) >= 0x20 && input.charCodeAt(j) <= 0x3f) j++
    if (j >= input.length) return input.length
    const body = input.slice(start, j)
    if (input[j] === 'm' && /^[0-9:;]*$/.test(body)) applySgr(body)
    return j + 1
  }

  /** 讀到字串結尾（BEL 或 ESC \）；回傳結尾之後的位置，並把 body 交給 onBody。 */
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
          // OSC 8：`8;<params>;<uri>`，uri 為空代表結束超連結。
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
        // 中間位元組（例如 `ESC ( B`）後接一個最終位元組。
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
      // 其他 C0／DEL／C1 控制字元（含 `\r`）丟掉。
      if (code >= 0x20 && code !== 0x7f && !(code >= 0x80 && code <= 0x9f)) text += ch
    }
    i++
  }
  cut()
  return out
}

/** 去掉所有跳脫序列與控制字元，只留可見文字（OSC 8 的顯示文字保留）。 */
export function stripAnsi(text: string): string {
  return parseAnsi(text).map((s) => s.text).join('')
}
