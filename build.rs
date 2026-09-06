//! 保证 `cargo build` 在未构建前端时也能成功。
//!
//! 管理后台由 `web/` 下的 Vite 产出并在编译期嵌入。如果开发者只想跑后端测试，
//! 不应该被迫先装 Node；这里放一个说明用的占位外壳，运行时会提示如何构建。

use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("web/dist");
    println!("cargo:rerun-if-changed=web/dist");

    if dist.join("index.html").exists() {
        return;
    }
    if let Err(error) = std::fs::create_dir_all(&dist) {
        println!("cargo:warning=无法创建 web/dist：{error}");
        return;
    }
    let placeholder = "<!doctype html><meta charset=\"utf-8\"><title>Akhub</title>\
<body style=\"font-family:system-ui;padding:2rem;line-height:1.7\">\
<h1>管理后台尚未构建</h1>\
<p>请执行 <code>cd web &amp;&amp; npm install &amp;&amp; npm run build</code>，然后重新编译 Akhub。</p>\
<p>网关的 <code>/v1</code> 接口不受影响，已经可以正常使用。</p>";
    if let Err(error) = std::fs::write(dist.join("index.html"), placeholder) {
        println!("cargo:warning=无法写入前端占位文件：{error}");
    } else {
        println!("cargo:warning=web/dist 不存在，已写入占位页面；请构建前端后重新编译");
    }
}
