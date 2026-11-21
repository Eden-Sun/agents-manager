import { toolsOfHost, useStore } from '../store/store'

export function useAgyMissing(host: string): boolean {
  return useStore((s) => toolsOfHost(s, host).agy?.installed === false)
}
