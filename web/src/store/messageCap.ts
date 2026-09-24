/**
 * issue #457：`MESSAGE_CAP` 是為了「UI 開一整天、WS 一路 append」而設的上限（issue #25），
 * 不是為了推翻使用者剛按下的「載入更早的」。
 *
 * 兩件事以前共用同一個常數，結果是：清單一到 500，補回來的那 200 則會在**下一則新訊息進來的
 * 瞬間**被 `capList` 從頭整批切掉（它留的是最新的 `cap` 筆），而使用者正捲在上面看那一段——
 * 畫面往後跳約 200 則，`moreMessages` 又被 `trimmed` 設回 `true`，看起來像剛才那一下沒生效。
 *
 * 現在每個對話有自己的**上限下緣**：使用者明確載入歷史之後，下緣抬到「補完的長度 ＋ `MESSAGE_CAP`」，
 * 也就是「你要的歷史全留著，另外還有 `MESSAGE_CAP` 則新訊息的餘裕才開始回收」。下緣只由使用者的
 * 動作抬高（按一次最多 `PAGE_SIZE`），所以清單仍然有界；整頁重灌就歸零。
 *
 * **為什麼是「抬高總上限」而不是「把載入的那段釘住、只剪後面的 live tail」**：清單必須是時間軸上
 * 連續的一段。從中間剪掉最舊的 live 訊息會在歷史與其餘訊息之間挖一個洞，畫面會從歷史直接跳到後面，
 * 比整批消失更難懂。所以只能從頭剪；而要不剪掉使用者要的歷史，唯一的辦法就是讓上限跟著長。
 */
import { MESSAGE_CAP } from './lists'

/** key 是 bot id 或 project id，跟 `moreMessages` 用同一組 key。 */
export type CapFloors = Record<string, number>

/** 這個對話現在的上限。沒被抬過就是 `MESSAGE_CAP`。 */
export function capFor(floors: CapFloors, key: string): number {
  const floor = floors[key]
  return typeof floor === 'number' && floor > MESSAGE_CAP ? floor : MESSAGE_CAP
}

/**
 * 使用者補回歷史之後的新下緣。`added` 是這一頁真的補進去的則數，`loaded` 是補完之後的清單長度。
 *
 * `added <= 0`（那一頁全是重複、或本來就沒有更早的）不抬：沒有新的歷史要保護，抬了只是讓上限
 * 隨著每次點擊往上飄。只升不降，所以連按幾次不會把前一次的餘裕吃掉。
 */
export function raiseFloor(floors: CapFloors, key: string, added: number, loaded: number): CapFloors {
  if (added <= 0) return floors
  const next = loaded + MESSAGE_CAP
  if (next <= capFor(floors, key)) return floors
  return { ...floors, [key]: next }
}

/** 整頁重灌（`loadMessages`／`loadGroupMessages`）：那一頁就是新的基準，餘裕歸零。 */
export function clearFloor(floors: CapFloors, key: string): CapFloors {
  if (floors[key] === undefined) return floors
  const out = { ...floors }
  delete out[key]
  return out
}
