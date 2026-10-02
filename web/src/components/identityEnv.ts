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
    const line = raw.trim()
    if (!line || line.startsWith('#')) return
    const n = idx + 1
    const i = line.indexOf('=')
    if (i < 0) {
      errors.push(`第 ${n} 行「${line}」沒有 =（要寫成 KEY=VALUE）`)
      return
    }
    const key = line.slice(0, i).trim()
    if (!key) {
      errors.push(`第 ${n} 行的 key 是空的（要寫成 KEY=VALUE）`)
      return
    }
    if (!ENV_NAME.test(key)) {
      errors.push(`第 ${n} 行 key「${key}」不是合法的環境變數名稱（只能英數與 _，不能數字開頭）`)
      return
    }
    if (key in env) errors.push(`第 ${n} 行 key「${key}」重複了（前面已經有一行）`)
    env[key] = line.slice(i + 1).trim()
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
