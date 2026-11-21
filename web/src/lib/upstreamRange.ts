import { cmpVersion } from './releaseTriage'

export interface RangeHost {
  host: string
  from: string | null
}

const plain = (v: string | null): v is string => v !== null && /^\d+(\.\d+)*$/.test(v.trim())

export function changelogRange(hosts: RangeHost[]): RangeHost {
  if (hosts.length === 0) return { host: 'local', from: null }
  if (!hosts.every((h) => plain(h.from))) return { host: hosts[0].host, from: null }
  const oldest = hosts.reduce((a, b) => (cmpVersion(b.from!.trim(), a.from!.trim()) < 0 ? b : a))
  return { host: oldest.host, from: oldest.from!.trim() }
}
