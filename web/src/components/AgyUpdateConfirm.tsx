/**
 * agy 更新的確認框內文（設定頁「更新檢查」與 header 的 agy 徽章共用，issue #1921）：兩處按下去都是 `installAgy(host)`，
 * 說明要一字不差，所以內文從這裡出；標題在 `lib/agyUpdate.ts`。
 */
export function AgyUpdateBody({ host, current, target }: { host: string; current: string | null; target: string }) {
  return (
    <>
      在 <strong>{host}</strong> 把 agy 從 {current ?? '?'} 更新到官方最新版（約 {target}）：從官方下載（約 50 MB）、驗 sha512 後換掉 <code>~/.local/bin/agy</code>；
      不跑官方 install.sh、<code>agy install</code> 或 <code>agy update</code>。跑著的 agy bot 不會被打斷，重啟後才換版。
    </>
  )
}
