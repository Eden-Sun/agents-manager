import { t } from '../lib/i18n'

/** `KEY=VALUE` 每行 → map；忽略空行與 `#` 註解。 */
export function parseEnvText(text: string): Record<string, string> {
  const out: Record<string, string> = {}
  for (const raw of text.split('\n')) {
    const line = raw.trim()
    if (!line || line.startsWith('#')) continue
    const i = line.indexOf('=')
    if (i <= 0) continue
    out[line.slice(0, i).trim()] = line.slice(i + 1).trim()
  }
  return out
}

export function envToText(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([k, v]) => `${k}=${v}`)
    .join('\n')
}

/**
 * 跟 Rust `str::trim`（`char::is_whitespace`＝Unicode White_Space）同一組空白：daemon 用它判斷 env 值是不是空的（`check_identity_env_values`）。
 * JS 的 `trim()` 集合不同：它多算 U+FEFF、少算 U+0085，同一個值前後端會判得不一樣。
 */
const RUST_WS = '\\t\\n\\v\\f\\r \\u0085\\u00a0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000'
const RUST_TRIM = new RegExp(`^[${RUST_WS}]+|[${RUST_WS}]+$`, 'g')
export const rustTrim = (s: string): string => s.replace(RUST_TRIM, '')

const ENV_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/

/**
 * 同 [`parseEnvText`]，但**不靜靜丟掉**打錯的行。daemon 對不合法的 env 名稱是用到時才濾掉（`tools::valid_env_name`），
 * 所以少打一個 `=`、名字帶空白，這個身份就變成「env 是空的」＝悄悄用了預設帳號，而且沒有任何錯誤。
 * `errors` 每條指出第幾行；合法的行照解（重複的 key 後面的贏），給預覽用。
 */
export function checkEnvText(text: string): { env: Record<string, string>; errors: string[] } {
  const env: Record<string, string> = {}
  const errors: string[] = []
  text.split('\n').forEach((raw, idx) => {
    const line = rustTrim(raw)
    if (!line || line.startsWith('#')) return
    const n = idx + 1
    const i = line.indexOf('=')
    if (i < 0) {
      errors.push(t('第 {n} 行「{line}」沒有 =（要寫成 KEY=VALUE）', { n, line }))
      return
    }
    const key = rustTrim(line.slice(0, i))
    if (!key) {
      errors.push(t('第 {n} 行的 key 是空的（要寫成 KEY=VALUE）', { n }))
      return
    }
    if (!ENV_NAME.test(key)) {
      errors.push(t('第 {n} 行 key「{key}」不是合法的環境變數名稱（只能英數與 _，不能數字開頭）', { n, key }))
      return
    }
    const value = rustTrim(line.slice(i + 1))
    // 值留空（`CLAUDE_CONFIG_DIR=`，多半是把 `$HOME/.claude-` 的尾巴刪光）：daemon 照存，身份就悄悄用了預設帳號。
    if (!value) {
      errors.push(t('第 {n} 行 {key} 的值是空的（空值等於沒設，身份會悄悄用預設帳號；要就寫上值，不要就刪掉這一行）', { n, key }))
      return
    }
    // `in` 會看到 Object.prototype（constructor、toString…），要用自己的屬性判斷；`__proto__` 要用 defineProperty 才會成為一般的 key。
    if (Object.hasOwn(env, key)) errors.push(t('第 {n} 行 key「{key}」重複了（前面已經有一行）', { n, key }))
    Object.defineProperty(env, key, { value, enumerable: true, writable: true, configurable: true })
  })
  return { env, errors }
}

const SECRET_KEY = /(KEY|TOKEN|SECRET|PASSW|CREDENTIAL)/i

/** 給畫面（列表、tooltip）看的 env：key／token／secret／password 類的值遮起來，其餘照顯示。 */
export function envDisplayText(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([k, v]) => `${k}=${SECRET_KEY.test(k) && v ? '••••••' : v}`)
    .join('\n')
}

/**
 * 設定檔裡既有的身份可能帶空值（#1750 之前存下的、或手寫的 `CLAUDE_CONFIG_DIR=`）：啟動時等於沒設，身份悄悄用預設帳號。
 * 回傳值為空或全空白的 key（規則同 daemon 的 `check_identity_env_values`）；設定頁只標示，不改寫 config.toml。
 */
export function emptyEnvKeys(env: Record<string, string>): string[] {
  return Object.entries(env)
    .filter(([, v]) => !rustTrim(v))
    .map(([k]) => k)
}
