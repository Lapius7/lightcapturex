// アイコンとマニフェスト（コモンコントロール v6 / Per-Monitor DPI）を埋め込む
fn main() {
    println!("cargo:rerun-if-changed=res");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app.res.o");
    let windres = if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") && cfg!(not(windows)) {
        "x86_64-w64-mingw32-windres"
    } else {
        "windres"
    };
    let ok = std::process::Command::new(windres)
        .args(["--input", "res/app.rc", "--output-format=coff", "--output"])
        .arg(&out)
        .args(["--include-dir", "res"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "windres failed");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
