import { useShallow } from 'zustand/react/shallow'
import { useStore } from '../store/store'
import { LOCAL_HOST } from '../api/types'
import { MemBadge } from './MemBadge'

export function HeadMemBadges() {
  const hosts = useStore(useShallow((s) => (s.mem?.hosts ?? []).map((h) => h.host)))
  if (hosts.length === 0) return null
  const remotes = hosts.filter((h) => h !== LOCAL_HOST)
  const ordered = hosts.includes(LOCAL_HOST) ? [LOCAL_HOST, ...remotes] : remotes
  const multi = ordered.length > 1
  const allHosts = multi ? ordered : undefined
  return (
    <div className={`head-ram${multi ? ' multi' : ''}`}>
      {ordered.map((host) => (
        <MemBadge key={host} host={host} hosts={allHosts} compact />
      ))}
    </div>
  )
}
