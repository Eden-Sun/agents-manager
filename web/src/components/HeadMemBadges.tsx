import { useShallow } from 'zustand/react/shallow'
import { t, useT } from '../lib/i18n'
import { projectHostName, useStore } from '../store/store'
import { LOCAL_HOST } from '../api/types'
import { MemBadge } from './MemBadge'

export function HeadMemBadges() {
  useT()
  const remotes = useStore((s) => (s.mem?.hosts ?? []).filter((h) => h.host !== LOCAL_HOST).map((h) => h.host).join('\n'))
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
            <span className="pane-badge head-host-panes" title={t('{host}：{n} 個 herdr pane', { host: host === LOCAL_HOST ? t('本機') : host, n: panes })}>
              <span className="pane-k">pane</span>
              <span className="pane-v">{panes}</span>
            </span>
            <MemBadge host={host} />
          </div>
        )
      })}
    </div>
  )
}
