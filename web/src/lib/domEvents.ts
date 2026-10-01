/** True when a bubbling event originated on the element handling it. */
export function eventIsFromCurrentTarget(event: { target: EventTarget | null; currentTarget: EventTarget }): boolean {
  return event.target === event.currentTarget
}

/** True when an event target is physically inside its current element (portals are outside). */
export function eventTargetIsInsideCurrentTarget(event: {
  target: EventTarget | null
  currentTarget: Element
}): boolean {
  return event.target !== null && event.currentTarget.contains(event.target as Node)
}

/**
 * True when a keydown came from a control that owns Enter／Space／arrows itself (a focused button, link, checkbox…).
 * A container's own shortcuts must skip these, or Enter on a focused 「取消」 button runs the container's Enter instead of the click.
 */
export function keyBelongsToControl(target: EventTarget | null): boolean {
  const el = target as { tagName?: string; type?: string } | null
  const tag = el?.tagName
  if (!tag) return false
  if (tag === 'BUTTON' || tag === 'A' || tag === 'SELECT' || tag === 'TEXTAREA') return true
  return tag === 'INPUT' && (el?.type === 'checkbox' || el?.type === 'radio' || el?.type === 'button' || el?.type === 'submit')
}
