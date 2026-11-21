import { msg } from './i18n'

export const BOT_NAME_HINT = msg('1–32 個字，不可含 @ , : ; 與看不見的字元，空白只能單一個、夾在中間')

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
