// Release profile 會在編譯時把前端嵌進 binary；只有這些 profile 要追蹤 web/dist 的變化。debug／test 使用執行期讀取。
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // rust-embed 沒開 debug-embed：debug assertions 開著的 profile（dev／test）不嵌入，不必因前端目錄變動重編。
    // release 更新前端時仍須重嵌，否則 binary 會帶舊 UI（#1071）。
    if !cfg!(debug_assertions) {
        println!("cargo:rerun-if-changed=../../web/dist");
    }
}
