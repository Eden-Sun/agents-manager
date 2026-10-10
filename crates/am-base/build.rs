// 前端嵌入（src/assets.rs 的 `rust_embed::Embed`）是在編譯時讀 `web/dist`。cargo 只看 .rs 與依賴，所以前端單獨重建
// （檔名 hash 變了、舊檔刪掉）時 am-base 不會重編，binary 還嵌著舊 UI（#1071）。盯著整個目錄：cargo 會掃它底下所有檔的
// mtime；目錄不存在（還沒建過前端）時 cargo 每次都重跑這支，也是對的。
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../web/dist");
}
