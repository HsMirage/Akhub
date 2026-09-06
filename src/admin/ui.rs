//! 内嵌的 React 管理后台（§六：不需要单独部署前端服务）。
//!
//! 产物由 `web/` 下的 Vite 构建生成，编译期整体嵌入二进制。资源文件名带内容
//! 哈希，因此可以长期强缓存；`index.html` 不能缓存，否则前端更新后用户会拿到
//! 旧壳子去加载已经不存在的资源。

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/web/dist"]
struct Assets;

/// 带内容哈希的资源可以放心长期缓存。
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// 应用外壳必须每次校验，否则前端升级后会加载到已被删除的旧资源。
const NO_CACHE: &str = "no-cache";

/// 提供 `/admin` 下的静态资源；路径未命中时回落到 SPA 外壳。
pub async fn serve(path: &str) -> Response {
    let trimmed = path.trim_start_matches('/');

    if !trimmed.is_empty()
        && let Some(asset) = Assets::get(trimmed)
    {
        let cache = if trimmed.starts_with("assets/") {
            IMMUTABLE
        } else {
            NO_CACHE
        };
        return (
            [
                (header::CONTENT_TYPE, content_type(trimmed)),
                (header::CACHE_CONTROL, cache),
            ],
            asset.data,
        )
            .into_response();
    }

    // 前端是单页应用：任何未知子路径都交给它自己处理路由。
    match Assets::get("index.html") {
        Some(shell) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, NO_CACHE),
            ],
            shell.data,
        )
            .into_response(),
        // 只有在跳过前端构建、直接 cargo build 时才会走到这里。
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "管理后台尚未构建。请在 web/ 目录执行 `npm install && npm run build` 后重新编译。",
        )
            .into_response(),
    }
}

/// 按扩展名给出 Content-Type。后台只会产出这几类文件，不值得引入 MIME 库。
fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/x-icon",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_cover_the_build_output() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type("assets/index-abc123.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type("assets/index-abc123.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type("unknown"), "application/octet-stream");
    }

    #[tokio::test]
    async fn unknown_paths_fall_back_to_the_spa_shell() {
        // 前端用 hash 路由，但直接访问 /admin/anything 也不该 404。
        let response = serve("/does-not-exist").await;
        assert_ne!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_shell_is_never_cached() {
        let response = serve("/").await;
        if response.status() == StatusCode::OK {
            assert_eq!(response.headers()[header::CACHE_CONTROL], NO_CACHE);
        }
    }
}
