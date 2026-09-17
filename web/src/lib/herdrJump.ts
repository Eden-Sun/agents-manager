/**
 * 從這個 app 跳進 herdr、直接落在那顆 bot 的 pane 上的一行指令（2026-09-17 使用者：
 * 「在 pane name show 出一行字，可以 resume 後進去至該 pane 的 herdr shell」）。
 *
 * 先 `agent focus <pane>`（herdr 0.8.2 實測：用 pane id 當 target 會連 workspace 一起切過去），
 * 再開 TUI 接回那個 session——TUI 一打開就是那顆 pane。focus 的 JSON 輸出丟掉，不然會洗在終端上。
 * 只給本機：遠端要先 ssh 到那台，focus 得在遠端跑，這裡組不出一條保證對的指令。
 */
const SAFE = /^[A-Za-z0-9_.:@%+=,/-]+$/

function q(s: string): string {
  return SAFE.test(s) ? s : `'${s.replace(/'/g, `'\\''`)}'`
}

export function herdrJumpCommand(session: string | null | undefined, paneId: string | null | undefined): string {
  const pane = paneId?.trim()
  if (!pane) return ''
  const sess = session?.trim() || 'agents-manager'
  return `herdr --session ${q(sess)} agent focus ${q(pane)} >/dev/null && herdr --session ${q(sess)}`
}
