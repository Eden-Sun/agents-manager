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
