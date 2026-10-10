/**
 * Bot 名字規則，跟 daemon `config::valid_bot_name` 同一份：1–32 字、不可含 `@ , : ;`；
 * 空白只能是單一個半形空白、夾在中間（2026-09-19 使用者：「bot name should be able to include space」）。
 */
export const BOT_NAME_HINT = '1–32 個字，不可含 @ , : ; 與看不見的字元，空白只能單一個、夾在中間'

// 控制字元與看不見的格式字元（零寬、方向控制、BOM）：跟 daemon `is_control()`＋`is_invisible_format_char` 同一組；`newDirName.ts` 也是這條。
// 從網頁複製貼上常夾帶 U+200B，前端不擋的話會是按了送出才被 daemon 回 400。
// eslint-disable-next-line no-control-regex
const HIDDEN_RE = /[\u0000-\u001f\u007f-\u009f\u200b-\u200f\u202a-\u202e\u2060-\u2064\u2066-\u2069\ufeff]/

export function isValidBotName(name: string): boolean {
  const n = [...name].length
  return (
    n >= 1 &&
    n <= 32 &&
    !name.startsWith(' ') &&
    !name.endsWith(' ') &&
    !name.includes('  ') &&
    !/[@,:;]/.test(name) &&
    !HIDDEN_RE.test(name) &&
    ![...name].some((c) => c !== ' ' && /\s/.test(c))
  )
}
