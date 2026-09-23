/** 代發訊息的來源標（`messages.relay_from`，SPEC §6.5d）。
 *
 * `relay_unverified`（issue #339）：呼叫端自稱是那顆 bot、卻沒帶它自己的 bot token 證明——相容期照收，
 * 但標籤要說「未驗證」，免得冒名的話看起來跟真的那顆說的一樣。daemon 哨符不會是未驗證（HTTP 帶不進來）。 */
export function relaySource(input: {
  fromId: string
  fromName: string
  toName: string
  unverified: boolean
}): { text: string; title: string; daemon: boolean } {
  const { fromId, fromName, toName, unverified } = input
  // `daemon` 是哨符不是 bot id（`agent_relay::DAEMON_SENDER`）：排程／daemon 自發，沒有人按過。
  if (fromId === 'daemon') {
    return { text: 'daemon 自動觸發', title: 'daemon 自動觸發的訊息（排程／自動化，不是人送的）', daemon: true }
  }
  const text = `${fromName || fromId}${toName ? ` → ${toName}` : ''}${unverified ? '（未驗證）' : ''}`
  const title = unverified
    ? '寄件端自稱是這顆 bot，但沒有帶它自己的 bot token 證明（不是你送的；來源未驗證）'
    : '由其他 agent 代為交辦的訊息（不是你送的）'
  return { text, title, daemon: false }
}
