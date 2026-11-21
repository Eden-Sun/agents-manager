import { t } from './i18n'

export function relaySource(input: {
  fromId: string
  fromName: string
  toName: string
  unverified: boolean
  cluster?: string | null
  fromIsAssignee?: boolean
  toIsAssignee?: boolean
}): { text: string; title: string; daemon: boolean } {
  const { fromId, fromName, unverified, cluster } = input
  if (fromId === 'daemon') {
    return { text: t('daemon 自動觸發'), title: t('daemon 自動觸發的訊息（排程／自動化，不是人送的）'), daemon: true }
  }
  const tag = (name: string, on?: boolean) => (cluster && on && name ? t('{name}（{cluster} 群集）', { name, cluster }) : name)
  const from = tag(fromName || fromId, input.fromIsAssignee)
  const toName = tag(input.toName, input.toIsAssignee)
  const text = unverified
    ? toName
      ? t('{from} → {to}（未驗證）', { from, to: toName })
      : t('{from}（未驗證）', { from })
    : toName
      ? t('{from} → {to}', { from, to: toName })
      : from
  const title = unverified
    ? t('寄件端自稱是這顆 bot，但沒有帶它自己的 bot token 證明（不是你送的；來源未驗證）')
    : t('由其他 agent 代為交辦的訊息（不是你送的）')
  return { text, title, daemon: false }
}
