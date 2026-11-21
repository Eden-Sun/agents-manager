export interface BlockedKeyContext {
  passthrough: boolean
  dangerous: boolean
  inside: boolean
  tag: string | null
  key: string
  defaultPrevented: boolean
}

export type BlockedKeyAction = 'pane' | 'close' | 'browser'

export function passthroughLive(passthrough: boolean, dangerous: boolean): boolean {
  return passthrough && !dangerous
}

export function blockedKeyAction(c: BlockedKeyContext): BlockedKeyAction {
  if (c.inside && (c.tag === 'input' || c.tag === 'textarea' || c.tag === 'select')) return 'browser'
  if (c.key === 'Tab') return 'browser'
  if (c.inside && (c.tag === 'button' || c.tag === 'a') && (c.key === 'Enter' || c.key === ' ')) return 'browser'
  if (!passthroughLive(c.passthrough, c.dangerous)) {
    return c.key === 'Escape' && !c.defaultPrevented ? 'close' : 'browser'
  }
  return 'pane'
}
