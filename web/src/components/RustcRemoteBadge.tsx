import { useStore } from '../store/store'
import { humanBytes } from './MemBadge'
import './memBadge.css'

function pct(n: number | null): string | null {
  if (n == null || !Number.isFinite(n)) return null
  return `${Math.round(n)}%`
}

/** 左上角、本機 RAM 旁邊：外部 rustc／Cargo 主機的 CPU 與 RAM（SPEC §15.1c）。沒開設定就不畫。 */
export function RustcRemoteBadge() {
  const row = useStore((s) => s.mem?.cargo_remote ?? null)
  if (!row) return null

  if (row.error) {
    return (
      <span className="mem-badge partial rustc-remote" title={`外部 rustc ${row.user}@${row.host}：量不到（${row.error}）`}>
        <span className="mem-k">rustc</span>
        <span className="mem-v">—</span>
      </span>
    )
  }

  const cpu = pct(row.cpu_pct) ?? pct(row.rustc_cpu_pct)
  const machine = row.machine
  const freePct = machine && machine.total_bytes > 0 ? (machine.available_bytes / machine.total_bytes) * 100 : null
  const low = freePct !== null && freePct < 15
  const hot = (row.cpu_pct ?? 0) >= 85 || (row.rustc_cpu_pct ?? 0) >= 400
  const tip = [
    `外部 rustc：${row.user}@${row.host}`,
    row.rustc_processes
      ? `rustc／cargo ${row.rustc_processes} 個行程 · ${humanBytes(row.rustc_bytes)}${row.rustc_cpu_pct != null ? ` · CPU ${Math.round(row.rustc_cpu_pct)}%` : ''}`
      : '現在沒有 rustc／cargo 在跑',
    ...(row.cpu_pct != null ? [`整機 CPU ${Math.round(row.cpu_pct)}%${row.nproc ? `／${row.nproc} 核` : ''}${row.load1 != null ? ` · load ${row.load1.toFixed(2)}` : ''}`] : []),
    ...(machine
      ? [
          `這台機器：剩 ${humanBytes(machine.available_bytes)} / 共 ${humanBytes(machine.total_bytes)}（已用 ${humanBytes(machine.total_bytes - machine.available_bytes)}）`,
        ]
      : []),
    '',
    '每 15 秒更新。不在 herdr 樹裡，所以本機 RAM 那格看不到它。',
  ]
  return (
    <span className={`mem-badge rustc-remote${low || hot ? ' mem-low' : ''}`} title={tip.join('\n')}>
      <span className="mem-k">rustc</span>
      {cpu ? <span className="mem-v">{cpu}</span> : null}
      <span className="mem-v">{row.rustc_bytes > 0 ? humanBytes(row.rustc_bytes) : machine ? humanBytes(machine.total_bytes - machine.available_bytes) : '0'}</span>
      {machine ? (
        <span className="mem-free" title={`外部編譯機還可用 ${humanBytes(machine.available_bytes)}，共 ${humanBytes(machine.total_bytes)}`}>
          剩 {humanBytes(machine.available_bytes)}
        </span>
      ) : null}
    </span>
  )
}
