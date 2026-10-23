/** 聊天室系統訊息的正文：Jev 留的（開頭「Jev 建議」）換成統一的 `JevLabel`＋只是建議的說明，其餘照原文（issue #1912）。 */
import { parseJevNotice } from '../lib/jevNotice'
import { JevLabel } from './JevLabel'

export function JevNoticeText({ text }: { text: string }) {
  const n = parseJevNotice(text)
  if (!n) return <>{text}</>
  return (
    <>
      <JevLabel prob={n.prob} note />
      {n.rest ? <> {n.rest}</> : null}
    </>
  )
}
