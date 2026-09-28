import { useShallow } from 'zustand/react/shallow'
import { projectHostName, useStore } from '../store/store'
import { LOCAL_HOST } from '../api/types'
import { MemBadge } from './MemBadge'

/**
 * 左上角 RAM 格（2026-09-28 使用者）：本機之外，`/api/mem` 量到的每台遠端（例：m4p）各一顆，
 * 不必切到那台的專案才看得到它還剩多少。一顆只講一台，不加總（SPEC §15）。有遠端時改成上下疊（`.multi`）。
 * 手機 menu 另在每顆前面放那台的 pane 數（2026-09-28 使用者）；桌機的 pane 總數在左邊，這格不重複。
 */
export function HeadMemBadges() {
  const remotes = useStore((s) => (s.mem?.hosts ?? []).filter((h) => h.host !== LOCAL_HOST).map((h) => h.host).join('\n'))
  // 同 Sidebar 的 PaneBadge：跑著的 bot 各算一個 pane，依專案所在主機分。回字串陣列給 useShallow。
  const paneHosts = useStore(
    useShallow((s) =>
      s.bots
        .filter((b) => {
          const r = s.runs[b.id]
          return Boolean(r) && r!.state !== 'stopped' && r!.state !== 'exited'
        })
        .map((b) => projectHostName(s, b.project_id)),
    ),
  )
  if (!remotes) {
    return (
      <div className="head-ram">
        <MemBadge />
      </div>
    )
  }
  const hosts = [LOCAL_HOST, ...remotes.split('\n')]
  return (
    <div className="head-ram multi">
      {hosts.map((host) => {
        const panes = paneHosts.filter((h) => h === host).length
        return (
          <div key={host} className="head-mem-row">
            <span className="pane-badge head-host-panes" title={`${host === LOCAL_HOST ? '本機' : host}：${panes} 個 herdr pane`}>
              <span className="pane-v">{panes}</span>
            </span>
            <MemBadge host={host} />
          </div>
        )
      })}
    </div>
  )
}
