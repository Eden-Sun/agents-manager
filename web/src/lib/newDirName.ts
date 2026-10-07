/**
 * 目錄選擇器「新資料夾」的名字規則（跟 daemon 的 `bot_input::check_new_dir_name` 同一份）：單一一段，不是路徑。
 * 前端先擋是為了立刻給使用者看得懂的提示；真正的把關在 daemon。回 `null`＝可以；否則是要顯示的原因。
 */
export function newDirNameProblem(raw: string): string | null {
  const name = raw.trim()
  if (!name) return '請輸入資料夾名稱'
  if (name === '.' || name === '..') return '名稱不能是 . 或 ..'
  if (name.includes('/')) return '名稱不能含「/」（一次只建一層；要進到別層請先在上面的清單走進去）'
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f-\u009f\u200b-\u200f\u202a-\u202e\u2060-\u2064\u2066-\u2069\ufeff]/.test(name)) return '名稱不能含控制或看不見的字元'
  if (new TextEncoder().encode(name).length > 255) return '名稱太長（最多 255 位元組）'
  return null
}
