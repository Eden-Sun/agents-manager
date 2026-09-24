/**
 * `BlockedModal` 的鍵盤要交給誰（issue #545）。純函式，因為這一條決定「按一下 `1` 會不會變成
 * 在 TUI 上按下 1. Yes」——那正是 #423 防誤刪框只准使用者本人核准的那一下。
 */
export interface BlockedKeyContext {
  /** 使用者有沒有打開鍵盤直通（預設關）。 */
  passthrough: boolean
  /** 畫面現在是防誤刪框：直通一律鎖死，開關也不給開。 */
  dangerous: boolean
  /** 事件來源在這個視窗裡面嗎。 */
  inside: boolean
  /** 事件目標的 tagName（小寫）；`null`＝沒有。 */
  tag: string | null
  key: string
  defaultPrevented: boolean
}

/**
 * `pane`＝送進終端、`close`＝關掉視窗、`browser`＝什麼都不做（交還給瀏覽器）。
 *
 * 直通關著時 Esc 關視窗（開著時 Esc 也要送進去，所以關閉只剩 ✕／點視窗外）。
 */
export type BlockedKeyAction = 'pane' | 'close' | 'browser'

/** 直通能不能用：防誤刪框上一律不能（#545）。 */
export function passthroughLive(passthrough: boolean, dangerous: boolean): boolean {
  return passthrough && !dangerous
}

export function blockedKeyAction(c: BlockedKeyContext): BlockedKeyAction {
  // 視窗裡的輸入控制項要能打字，否則字會跑進背後的聊天輸入框。
  if (c.inside && (c.tag === 'input' || c.tag === 'textarea' || c.tag === 'select')) return 'browser'
  if (c.key === 'Tab') return 'browser'
  // 焦點在視窗內的按鈕上時 Enter／Space 要啟動那顆按鈕，而不是送進 pane。
  if (c.inside && (c.tag === 'button' || c.tag === 'a') && (c.key === 'Enter' || c.key === ' ')) return 'browser'
  if (!passthroughLive(c.passthrough, c.dangerous)) {
    return c.key === 'Escape' && !c.defaultPrevented ? 'close' : 'browser'
  }
  return 'pane'
}
