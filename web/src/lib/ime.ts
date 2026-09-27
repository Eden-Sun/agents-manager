/** IME confirmation can arrive after compositionend, especially in WebKit. */
export function isImeEnter(e: Pick<KeyboardEvent, 'isComposing' | 'keyCode'>): boolean {
  return e.isComposing || e.keyCode === 229
}
