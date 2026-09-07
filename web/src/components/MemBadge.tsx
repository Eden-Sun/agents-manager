import { useStore } from '../store/store'
import { LOCAL_HOST } from '../api/types'
import { MemPopover } from './MemPopover'

/** 1.4G / 820M / 64M — 一格寬度就要看得懂，所以個位數才給小數。 */
export function humanBytes(n: number): string {
  if (n <= 0) return '0'
  const g = n / 1024 ** 3
  if (g >= 1) return `${g < 10 ? g.toFixed(1) : Math.round(g)}G`
  const m = n / 1024 ** 2
  if (m >= 1) return `${Math.round(m)}M`
  return `${Math.max(1, Math.round(n / 1024))}K`
}

/**
 * 一台主機上 herdr 進程樹現在佔的常駐記憶體（SPEC §15）。
 *
 * **一格只講一台**。左上角固定是本機——那是「我這台現在多重」，隨時都想知道；遠端主機的
 * 數字只在你正在看那台上面的東西時才出現（bot / 群組 / team 的標題列），不然一個平常用不到
 * 的數字會一直佔著版面，而且把兩台加總出來的那個大數字也不好懂（8G 裡有 5G 是別台的）。
 */
export function MemBadge({ host = LOCAL_HOST, onlyRemote = false }: { host?: string; onlyRemote?: boolean }) {
  const row = useStore((s) => s.mem?.hosts.find((h) => h.host === host) ?? null)
  const remote = host !== LOCAL_HOST
  // 標題列傳 `onlyRemote`：本機的數字左上角已經有一顆，同一畫面掛兩次一樣的數字沒有意義。
  if (onlyRemote && !remote) return null
  if (!row) return null

  if (row.error) {
    // 量不到就直說。這裡沒有「總和」可以被悄悄拉低，所以標示比隱藏有用。
    return (
      <span className="mem-badge partial" title={`${host}：量不到 herdr 的記憶體（${row.error}）`}>
        {remote ? <span className="mem-k">@{host}</span> : null}
        <span className="mem-v">—</span>
      </span>
    )
  }
  if (row.total_bytes === 0 && row.processes === 0) return null

  const tip = [
    `${remote ? host : '本機'}：herdr 進程樹現在佔用的記憶體`,
    `herdr 本身 ${humanBytes(row.herdr_bytes)} · 底下的 pane 與 CLI ${humanBytes(row.agents_bytes)}`,
    `${row.processes} 個 process`,
    '',
    '算的是「跑在 herdr pane 裡的一切」，不只是這裡管的 bot。',
    '每 15 秒更新一次。',
  ]
  // 點得開：一個總數看不出「哪些是我自己開的、可以砍」，明細見 `MemPopover`（SPEC §15.2）。
  return (
    <MemPopover host={host}>
      <span className={`mem-badge${remote ? ' remote' : ''}`} title={`${tip.join('\n')}\n\n點一下看有哪些程序。`}>
        <span className="mem-k">{remote ? `@${host}` : 'RAM'}</span>
        <span className="mem-v">{humanBytes(row.total_bytes)}</span>
      </span>
    </MemPopover>
  )
}
