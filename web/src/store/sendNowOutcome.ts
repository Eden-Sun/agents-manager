/**
 * 插隊送出（#103、#120）的 200 裡，`send_now` 有兩個值是「沒插成」：
 * - `not_sent`：送出鍵沒生效（打字沒回應、herdr 拒收那顆鍵）。這一則 `delivery:"failed"`，字可能還留在終端的輸入框，
 *   正在跑的回合照常。
 * - `unknown`：不知道送出鍵有沒有生效。這一則 `delivery:"unknown"`（turn 本身已是 failed、不佔 in-flight，沒有回合可「放棄」），
 *   正在跑的回合先不收、等證據；那一則若其實送出去了，回覆會以外部回合出現。
 *
 * 兩個都**沒有**照一般方式送出，也不是「claude 太舊」——那是 409 `send_now_refused` 的事。
 */
export interface SendNowFellThrough {
  text: string
  /** 輸入框的字算不算已經交出去：`unknown` 不能叫人重送（可能已經送出），`not_sent` 明確沒送，字留著。 */
  consumed: boolean
}

export function sendNowFellThrough(sendNow: string | null | undefined): SendNowFellThrough | null {
  if (sendNow === 'not_sent') {
    return {
      text: '插隊沒有生效：送出鍵沒有作用，這一則沒送出，正在跑的回合照常。字可能還留在終端的輸入框，先到「終端」分頁清掉再送。',
      consumed: false,
    }
  }
  if (sendNow === 'unknown') {
    return {
      text: '插隊送出的結果不明：送出鍵送出去了，但不知道有沒有生效。正在跑的回合先不動；這一則若其實送出去了，回覆會以外部回合出現。先別重送。',
      consumed: true,
    }
  }
  return null
}
