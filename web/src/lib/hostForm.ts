/**
 * 新增主機表單的欄位檢查，跟 daemon 同一條規則（`config::host_target_problem`、`POST /hosts`）：
 * 前端放行、daemon 拒絕時使用者只看到一句 400；反過來前端擋得比 daemon 嚴就是在騙人。回傳 `null`＝沒問題。
 */

/** ssh 目標（可用 ssh_config 別名）：開頭 `-` 會被 ssh 當成選項（`-oProxyCommand=…` 在本機執行命令），空白／控制字元永遠不是合法的目標。 */
export function sshTargetProblem(ssh: string): string | null {
  const hostPart = ssh.includes('@') ? ssh.slice(ssh.lastIndexOf('@') + 1) : ssh
  if (!ssh || ssh.startsWith('-') || hostPart.startsWith('-') || /[\s\u0000-\u001f\u007f]/.test(ssh)) {
    return 'ssh 目標要是主機（或 user@host、ssh_config 別名）：不能空、不能以 - 開頭、不能有空白'
  }
  return null
}

/** herdr session 名字：1–64 字、英數開頭、只有英數與 . _ -。 */
export function sessionProblem(session: string): string | null {
  if (session.length > 64 || !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(session)) {
    return 'herdr_session 要是 1–64 個字，英數開頭，只能有英數與 . _ -'
  }
  return null
}

/** port：1–65535 的整數。打錯不能悄悄變成預設值（以前 `Number(x) || 22`：「2222x」連到 22）。 */
export function portProblem(text: string): string | null {
  const t = text.trim()
  if (!/^\d{1,5}$/.test(t) || Number(t) < 1 || Number(t) > 65535) return 'port 要是 1–65535 的整數'
  return null
}
