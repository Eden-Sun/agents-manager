import { toolsOfHost, useStore } from '../store/store'

/** 這台主機 tools 探測已知沒有 agy（還沒探測到或探測不出來的 `null` 不算）。 */
export function useAgyMissing(host: string): boolean {
  return useStore((s) => toolsOfHost(s, host).agy?.installed === false)
}
