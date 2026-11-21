import { t } from '../lib/i18n'

export interface SendNowFellThrough {
  text: string
  consumed: boolean
}

export function sendNowFellThrough(sendNow: string | null | undefined): SendNowFellThrough | null {
  if (sendNow === 'not_sent') {
    return {
      text: t('插隊沒有生效：送出鍵沒有作用，這一則沒送出，正在跑的回合照常。字可能還留在終端的輸入框，先到「終端」分頁清掉再送。'),
      consumed: false,
    }
  }
  if (sendNow === 'unknown') {
    return {
      text: t('插隊送出的結果不明：送出鍵送出去了，但不知道有沒有生效。正在跑的回合先不動；這一則若其實送出去了，回覆會以外部回合出現。先別重送。'),
      consumed: true,
    }
  }
  return null
}
