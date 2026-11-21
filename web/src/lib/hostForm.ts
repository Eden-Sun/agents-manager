import { t } from './i18n'
import { hasCodeIn } from './charCodes'

export function sshTargetProblem(ssh: string): string | null {
  const hostPart = ssh.includes('@') ? ssh.slice(ssh.lastIndexOf('@') + 1) : ssh
  if (!ssh || ssh.startsWith('-') || hostPart.startsWith('-') || /\s/.test(ssh) || hasCodeIn(ssh, 0, 0x1f) || hasCodeIn(ssh, 0x7f, 0x9f)) {
    return t('ssh 目標要是主機（或 user@host、ssh_config 別名）：不能空、不能以 - 開頭、不能有空白')
  }
  return null
}

export function sessionProblem(session: string): string | null {
  if (session.length > 64 || !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(session)) {
    return t('herdr_session 要是 1–64 個字，英數開頭，只能有英數與 . _ -')
  }
  return null
}

export function portProblem(text: string): string | null {
  const v = text.trim()
  if (!/^\d{1,5}$/.test(v) || Number(v) < 1 || Number(v) > 65535) return t('port 要是 1–65535 的整數')
  return null
}
