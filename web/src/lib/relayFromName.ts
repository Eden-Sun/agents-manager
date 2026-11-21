import { recallRelayName, rememberRelayName } from './relayNameMemo.ts'
import { msg, tr } from './i18n'

const RETIRED = () => tr(msg('（已收掉）'))

export function relayFromName(input: { fromId: string; liveName: string; savedName?: string | null; deleted?: boolean }): string {
  const { fromId, liveName, savedName, deleted } = input
  if (liveName) {
    rememberRelayName(fromId, liveName)
    return deleted ? `${liveName}${RETIRED()}` : liveName
  }
  if (savedName) return deleted ? `${savedName}${RETIRED()}` : savedName
  const remembered = recallRelayName(fromId)
  return remembered ? `${remembered}${RETIRED()}` : ''
}
