import { recallRelayName, rememberRelayName } from './relayNameMemo.ts'

const RETIRED = '（已收掉）'

/** 代發來源的顯示名，依序找：
 * 1. 清單找得到的活 bot → 用清單的名字（順手記住）；`deleted` 為真才標已收掉。
 * 2. 清單找不到：daemon 補的 `savedName`（活著就照樣顯示，已收掉才標）。
 * 3. 都沒有：曾經解析到的名字（記憶），標已收掉。
 * 以上都沒有才回空字串，呼叫端退回 ID。 */
export function relayFromName(input: { fromId: string; liveName: string; savedName?: string | null; deleted?: boolean }): string {
  const { fromId, liveName, savedName, deleted } = input
  if (liveName) {
    rememberRelayName(fromId, liveName)
    return deleted ? `${liveName}${RETIRED}` : liveName
  }
  if (savedName) return deleted ? `${savedName}${RETIRED}` : savedName
  const remembered = recallRelayName(fromId)
  return remembered ? `${remembered}${RETIRED}` : ''
}
