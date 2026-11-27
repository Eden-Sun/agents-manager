import { useShallow } from 'zustand/react/shallow'
import { useStore } from '../store/store'
import { LOCAL_HOST } from '../api/types'
import { BuildSlotsChip } from './BuildSlotsChip'
import { MemBadge } from './MemBadge'

export function HeadMemBadges() {
  const hosts = useStore(useShallow((s) => (s.mem?.hosts ?? []).map((h) => h.host)))
  const remotes = hosts.filter((h) => h !== LOCAL_HOST)
  const multi = remotes.length > 0
  const allHosts = multi ? [LOCAL_HOST, ...remotes] : undefined
  return (
    <div className={`head-ram${multi ? ' multi' : ''}`}>
      <div className="head-host-row local">
        <MemBadge host={LOCAL_HOST} hosts={allHosts} />
        <BuildSlotsChip />
      </div>
      {remotes.map((host) => (
        <div key={host} className="head-host-row">
          <MemBadge host={host} hosts={allHosts} />
        </div>
      ))}
    </div>
  )
}
