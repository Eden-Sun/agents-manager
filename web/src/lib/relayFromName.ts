/** 代發來源的顯示名。活著的 bot 直接用前端清單的名字；已收掉的 bot 不在清單裡，改用 daemon 補的名字並標「已收掉」。
 * 兩個都沒有才回空字串，呼叫端退回 ID。 */
export function relayFromName(input: { liveName: string; savedName?: string | null; deleted?: boolean }): string {
  const name = input.liveName || input.savedName || ''
  if (!name) return ''
  return input.deleted ? `${name}（已收掉）` : name
}
