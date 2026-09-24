/**
 * 同一條 WS 連線上，這一幀是不是已經送過了（issue #521）。
 *
 * daemon 的 `ws_loop` 先訂閱再讀重播環，兩者之間送出的耐久事件兩邊都有；daemon 端已經擋掉，這是第二道。
 * 擋它的理由是 handler 不見得冪等：`message_added` 靠 id 去重沒事，但 `bots_restart_progress` 是純累加
 * （`restartBatch.ts` 的 `done + 1`、`ok` 追加一筆），重複就是多算一次、一鍵重啟的進度會超前。
 *
 * **高水位是「這條連線」的，不是 store 的 `lastSeq`。** 用 `lastSeq` 會在 daemon 重啟後出事：那時 seq 從頭
 * 數起，而手上的 `lastSeq` 還停在舊 daemon 的大號碼，新 daemon 那些號碼很小的幀會被整段擋掉——正是 issue #368
 * 要補的那一段。每次 socket 開起來歸零就沒有這個問題，daemon 端 `sent_through` 從 0 起算也是同一個理由。
 *
 * `turn_progress` 不進重播環，所以不可能重複，不必也不應該參與（它的 seq 會把水位推過頭）；`resync` 帶的是
 * daemon 當下的 seq，更不能擋掉。兩者都照舊放行、也不推進水位——跟 `seqAfterFrame` 的「耐久」是同一條線。
 */
export type FrameGate = { skip: boolean; seen: number }

export function gateFrame(seen: number, type: string, seq: number | undefined): FrameGate {
  if (typeof seq !== 'number' || type === 'turn_progress' || type === 'resync') return { skip: false, seen }
  if (seq <= seen) return { skip: true, seen }
  return { skip: false, seen: seq }
}
