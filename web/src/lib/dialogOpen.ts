export function dialogOpen(root: ParentNode = document): boolean {
  return [...root.querySelectorAll('.modal-backdrop, .confirm-backdrop, .lightbox, [aria-modal="true"]')].some((el) => !el.matches('aside.sidebar'))
}
