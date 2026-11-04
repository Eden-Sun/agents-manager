import type { CSSProperties, ReactNode } from 'react'
import { parseAnsi, type AnsiSegment, type AnsiStyle } from '../lib/ansiSgr'
import { termPieces } from '../lib/termPieces'
import { fitRules } from '../lib/termRules'
import { TermLink } from './TermLink'
import './ansiTerm.css'

/**
 * 帶 ANSI 顏色的終端文字 → React 節點。`<pre className="term">` 裡直接放：`<AnsiTerm text={body} columns={snap.columns} />`。
 *
 * 流程：解析跳脫序列（`parseAnsi`）→ 逐行套 `fitRules`（分隔線縮短，跟 `linkifyTerm` 一致）→ 對去掉顏色的純文字跑 `termPieces` 找網址
 * → 再把網址範圍切回原本的色段。所以網址跨顏色也能點：每一段色段各自是一個 `TermLink`，點了都複製整條網址。
 * 網址與色段的切分都照字元位置做，不靠對齊的字串，所以 `termPieces` 的規則（折行接回、標點不算網址）原樣生效。
 */
export function AnsiTerm({ text, columns }: { text: string; columns?: number | null }) {
  return <span className="ansi-term">{renderAnsi(text, columns)}</span>
}

type Run = AnsiSegment

/** 片段依 `\n` 切成行；每一行是一串 run。 */
function splitLines(segs: AnsiSegment[]): Run[][] {
  const lines: Run[][] = [[]]
  for (const s of segs) {
    s.text.split('\n').forEach((part, k) => {
      if (k > 0) lines.push([])
      if (part) lines[lines.length - 1].push({ ...s, text: part })
    })
  }
  return lines
}

/** 取 [start, end) 範圍內的 run（字元位置相對於整行）。 */
function sliceRuns(runs: Run[], start: number, end: number): Run[] {
  const out: Run[] = []
  let at = 0
  for (const r of runs) {
    const from = Math.max(start, at)
    const to = Math.min(end, at + r.text.length)
    if (from < to) out.push({ ...r, text: r.text.slice(from - at, to - at) })
    at += r.text.length
    if (at >= end) break
  }
  return out
}

function colorCss(c: string | undefined): string | undefined {
  if (!c) return undefined
  return c.startsWith('ansi:') ? `var(--ansi-${c.slice(5)})` : c
}

/** 樣式 → className＋inline style。沒有任何樣式時回傳 undefined（直接放字串，不包 span）。 */
function styleProps(style: AnsiStyle): { className?: string; style?: CSSProperties } | undefined {
  let fg = colorCss(style.fg)
  let bg = colorCss(style.bg)
  if (style.inverse) {
    const f = fg ?? 'var(--term-text)'
    fg = bg ?? 'var(--term-bg)'
    bg = f
  }
  const cls = [
    style.bold && 'ansi-b',
    style.dim && 'ansi-dim',
    style.italic && 'ansi-i',
    style.underline && 'ansi-u',
    style.strike && 'ansi-s',
  ].filter(Boolean)
  if (!fg && !bg && cls.length === 0) return undefined
  const css: CSSProperties = {}
  if (fg) css.color = fg
  if (bg) css.backgroundColor = bg
  return { className: cls.length ? cls.join(' ') : undefined, style: fg || bg ? css : undefined }
}

function renderRun(r: Run, url: string | null, key: number): ReactNode {
  const link = url ?? r.href ?? null
  const props = styleProps(r.style)
  if (!props && !link) return r.text
  const inner: ReactNode = link ? <TermLink url={link} text={r.text} /> : r.text
  if (!props) return <TermLink key={key} url={link!} text={r.text} />
  return (
    <span key={key} className={props.className} style={props.style}>
      {inner}
    </span>
  )
}

function renderAnsi(raw: string, columns?: number | null): ReactNode[] {
  const lines = splitLines(parseAnsi(raw))
  const plainLines = lines.map((runs) => runs.map((r) => r.text).join(''))
  // 分隔線縮短：`fitRules` 只保留前綴，所以截掉的部分就是 run 的尾端。
  plainLines.forEach((line, i) => {
    const fitted = fitRules(line)
    if (fitted.length !== line.length) {
      lines[i] = sliceRuns(lines[i], 0, fitted.length)
      plainLines[i] = fitted
    }
  })
  const plain = plainLines.join('\n')
  const rows = /https?:\/\//.test(plain) ? termPieces(plain, columns) : null

  const out: ReactNode[] = []
  let key = 0
  lines.forEach((runs, i) => {
    if (i > 0) out.push('\n')
    if (!rows) {
      for (const r of runs) out.push(renderRun(r, null, key++))
      return
    }
    let pos = 0
    for (const p of rows[i] ?? []) {
      for (const r of sliceRuns(runs, pos, pos + p.text.length)) out.push(renderRun(r, p.url, key++))
      pos += p.text.length
    }
    for (const r of sliceRuns(runs, pos, Number.POSITIVE_INFINITY)) out.push(renderRun(r, null, key++))
  })
  return out
}
