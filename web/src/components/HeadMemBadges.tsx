import { useStore } from '../store/store'
import { LOCAL_HOST } from '../api/types'
import { MemBadge } from './MemBadge'

/**
 * 左上角 RAM 格（2026-09-28 使用者）：本機之外，`/api/mem` 量到的每台遠端（例：m4p）各一顆，
 * 不必切到那台的專案才看得到它還剩多少。一顆只講一台，不加總（SPEC §15）。有遠端時改成上下疊（`.multi`）。
 */
export function HeadMemBadges() {
  const remotes = useStore((s) => (s.mem?.hosts ?? []).filter((h) => h.host !== LOCAL_HOST).map((h) => h.host).join('\n'))
  return (
    <div className={`head-ram${remotes ? ' multi' : ''}`}>
      <MemBadge />
      {remotes ? remotes.split('\n').map((host) => <MemBadge key={host} host={host} />) : null}
    </div>
  )
}
