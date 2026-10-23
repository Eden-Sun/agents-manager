/**
 * 排程編輯器的時區下拉會列出 `Intl.supportedValuesOf('timeZone')` 的全部（約 445 個）`<option>`，
 * 在 happy-dom 裡每次開編輯器、每次重繪都要走一遍，一次動作約 0.2 秒。測試要的是「編輯器怎麼運作」，不是 445 個選項本身：
 * 換成一份短清單（仍含測試用到的 `Asia/Taipei`；`UTC` 與目前值由元件自己加），回傳 `restore()` 還原。
 */
export function shortTimeZoneList(): { restore: () => void } {
  const intl = Intl as unknown as { supportedValuesOf?: (key: string) => string[] }
  const real = intl.supportedValuesOf
  intl.supportedValuesOf = (key: string) => (key === 'timeZone' ? ['Asia/Taipei', 'Asia/Tokyo', 'America/New_York', 'Europe/London'] : real?.(key) ?? [])
  return {
    restore: () => {
      intl.supportedValuesOf = real
    },
  }
}
