/**
 * 畫面上有沒有疊在最上面的對話框（全域快捷鍵與抽屜的 Esc 要讓路）。
 * 認 `.modal-backdrop`／`.confirm-backdrop`／圖片燈箱 `.lightbox`，以及任何 `aria-modal="true"`——只認前兩個 class 的話，
 * 燈箱（它沒有 backdrop）開著時 ⌥↑／Ctrl+1 還是會在背後換 bot／專案。手機的側邊欄抽屜（`aside.sidebar`，開著時
 * 自己帶 `aria-modal`）不算：它不是「蓋在抽屜上面」的對話框，抽屜自己的 Esc 與 Ctrl+1 跳專案都靠這個例外。
 */
export function dialogOpen(root: ParentNode = document): boolean {
  return [...root.querySelectorAll('.modal-backdrop, .confirm-backdrop, .lightbox, [aria-modal="true"]')].some((el) => !el.closest('aside.sidebar'))
}
