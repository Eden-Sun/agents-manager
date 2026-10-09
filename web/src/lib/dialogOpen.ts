/**
 * 畫面上有沒有疊在最上面的對話框（全域快捷鍵與抽屜的 Esc 要讓路）。
 * 認 `.modal-backdrop`／`.confirm-backdrop`／圖片燈箱 `.lightbox`，以及任何 `aria-modal="true"`——只認前兩個 class 的話，
 * 燈箱（它沒有 backdrop）開著時 ⌥↑／Ctrl+1 還是會在背後換 bot／專案。手機的側邊欄抽屜**本身**（`aside.sidebar`，開著時
 * 自己帶 `aria-modal`）不算：它不是「蓋在抽屜上面」的對話框，抽屜自己的 Esc 與 Ctrl+1 跳專案都靠這個例外。
 * 但只排除抽屜本身，不排除它**裡面**的東西：側欄裡開的「新增 Project／新增 Bot／環境設定」（`<Modal>` 沒有 portal，DOM 在
 * `aside.sidebar` 底下）是真的疊在抽屜上面的對話框，必須算（#935：用 `closest` 排除整個側欄時，Esc／上一頁關掉的是底下的抽屜，
 * 對話框跟著藏進滑出去的側欄；桌機 ⌥↓／Ctrl+1 也會在它背後換 bot）。
 */
export function dialogOpen(root: ParentNode = document): boolean {
  return [...root.querySelectorAll('.modal-backdrop, .confirm-backdrop, .lightbox, [aria-modal="true"]')].some((el) => !el.matches('aside.sidebar'))
}
