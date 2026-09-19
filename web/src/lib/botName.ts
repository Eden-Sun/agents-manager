/**
 * Bot 名字規則，跟 daemon `config::valid_bot_name` 同一份：1–32 字、不可含 `@ , : ;`；
 * 空白只能是單一個半形空白、夾在中間（2026-09-19 使用者：「bot name should be able to include space」）。
 */
export const BOT_NAME_HINT = '1–32 個字，不可含 @ , : ;，空白只能單一個、夾在中間'

export function isValidBotName(name: string): boolean {
  const n = [...name].length
  return (
    n >= 1 &&
    n <= 32 &&
    !name.startsWith(' ') &&
    !name.endsWith(' ') &&
    !name.includes('  ') &&
    !/[@,:;]/.test(name) &&
    ![...name].some((c) => c !== ' ' && /\s/.test(c))
  )
}
