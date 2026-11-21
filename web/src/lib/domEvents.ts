export function eventIsFromCurrentTarget(event: { target: EventTarget | null; currentTarget: EventTarget }): boolean {
  return event.target === event.currentTarget
}

export function eventTargetIsInsideCurrentTarget(event: {
  target: EventTarget | null
  currentTarget: Element
}): boolean {
  return event.target !== null && event.currentTarget.contains(event.target as Node)
}

export function keyBelongsToControl(target: EventTarget | null): boolean {
  const el = target as { tagName?: string; type?: string } | null
  const tag = el?.tagName
  if (!tag) return false
  if (tag === 'BUTTON' || tag === 'A' || tag === 'SELECT' || tag === 'TEXTAREA') return true
  return tag === 'INPUT' && (el?.type === 'checkbox' || el?.type === 'radio' || el?.type === 'button' || el?.type === 'submit')
}
