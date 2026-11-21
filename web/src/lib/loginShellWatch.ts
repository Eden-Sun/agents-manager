const watched = new Set<string>()

const keyOf = (host: string, paneId: string) => `${host || 'local'}/${paneId}`

export function watchLoginShell(host: string, paneId: string): void {
  watched.add(keyOf(host, paneId))
}

export function isLoginShell(host: string, paneId: string): boolean {
  return watched.has(keyOf(host, paneId))
}

export function forgetLoginShell(host: string, paneId: string): void {
  watched.delete(keyOf(host, paneId))
}

const SUCCESS_RE = /\b(successfully logged in|logged in successfully|login successful|logged in as\b)/i

export function loginSucceeded(text: string): boolean {
  return SUCCESS_RE.test(text)
}
