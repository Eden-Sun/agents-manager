import type { CloseNeedsConfirm } from '../api'

/** 關 pane／結束 shell 被 daemon 擋下（服務 pane、或讀不到它在跑什麼）時，第二道確認框的說明（AGM 驗收 9f05b03）。 */
export function ServicePaneNote({ needs }: { needs: CloseNeedsConfirm | null }) {
  if (needs?.unverified) {
    return <>daemon 讀不到這顆 pane 現在在跑什麼，無法確認裡面沒有 dev server 之類的服務。確定要關嗎？</>
  }
  return (
    <>
      這顆 pane 正在 listen
      {needs?.pane?.listen_ports.length ? (
        <>
          {' '}
          <strong>{needs.pane.listen_ports.join('、')}</strong>
        </>
      ) : null}
      {needs?.pane?.foreground ? (
        <>
          （<code>{needs.pane.foreground}</code>）
        </>
      ) : null}
      。關掉它等於把裡面的服務一起停掉。確定要關嗎？
    </>
  )
}
