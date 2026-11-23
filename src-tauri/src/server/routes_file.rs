use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;
use tracing::error;

use crate::server::auth::Caller;
use crate::state::AppState;

/// GET /file?share=<id>&path=<relative_path>&offset=<u64>&len=<u64>
///
/// Streams file bytes. `offset` seeks into the file (default 0),
/// `len` caps the number of bytes returned (default: to EOF).
/// Supports partial reads for F-60 Range-style access.
pub async fn file_handler(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let share_id = params
        .get("share")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "Missing 'share' parameter".into()))?;
    let rel_path = params
        .get("path")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "Missing 'path' parameter".into()))?;

    let offset = params
        .get("offset")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    let len = params.get("len").and_then(|v| v.parse::<u64>().ok());

    let share_root = {
        let config = state.config.lock();
        let share = config
            .shares
            .iter()
            // 不可见与不存在都回 404：不向对端泄露「本机有这么个共享项」。
            .find(|s| s.id == *share_id && caller.sees_share(&state, s))
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    format!("Share '{}' not found", share_id),
                )
            })?;
        share.path.clone()
    };

    // 先校验再 `join`：Windows 上绝对/UNC 路径会让 `join` 清空共享项基址，`canonicalize`
    // 会真的去打开那个网络路径（SMB 认证），然后才轮到前缀检查。
    if !crate::server::safe_relative_share_path(rel_path) {
        return Err((StatusCode::FORBIDDEN, "Path access denied".into()));
    }

    // 空 `path` = 共享项根自身。**不能**写成 `share_root.join("")`：那样会带上结尾分隔符
    // （`F:\x\a.txt\`），`canonicalize` 对文件立刻报 `ERROR_INVALID_NAME`(os 267) → 本 handler
    // 回 403，对端看到的就是"起拖成功、落下报未指定的错误、文件始终不出现"（拖的正是
    // "共享项根就是一个文件"这一类）。`/stat`、`/tree` 都有这条空路径判断，只有这里漏了。
    let target = if rel_path.is_empty() {
        share_root.clone()
    } else {
        share_root.join(rel_path)
    };

    // Path traversal guard: canonicalize and ensure it stays under the share root.
    let canonical_target = target.canonicalize().map_err(|e| {
        error!("Failed to canonicalize {target:?}: {e}");
        (StatusCode::FORBIDDEN, "Path access denied".into())
    })?;
    let canonical_root = share_root.canonicalize().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resolve share root".into(),
        )
    })?;

    if !canonical_target.starts_with(&canonical_root) {
        return Err((StatusCode::FORBIDDEN, "Path traversal denied".into()));
    }

    if !canonical_target.is_file() {
        return Err((StatusCode::BAD_REQUEST, "Path is not a file".into()));
    }

    let mut file = tokio::fs::File::open(&canonical_target)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("File not found: {e}")))?;

    let file_size = file
        .metadata()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read metadata: {e}"),
            )
        })?
        .len();

    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Seek failed: {e}"),
            )
        })?;
    }

    // 越过 EOF 的 offset 必须明确报错，不能回 200 + 0 字节：客户端把“空包”当 EOF，
    // 会把手里那份前缀原子改名成最终文件 —— 对端文件在传输中变小/被替换时，用户就得到
    // 一份“长度短了但界面显示成功”的截断下载。`offset == file_size` 仍是合法 EOF。
    if offset > file_size {
        return Err((
            StatusCode::RANGE_NOT_SATISFIABLE,
            format!("offset {offset} 超过文件大小 {file_size}（文件可能已变化）"),
        ));
    }
    let remaining = file_size.saturating_sub(offset);
    let read_len = len.map(|l| l.min(remaining)).unwrap_or(remaining);

    let limited = file.take(read_len);
    // 帧容量只影响**分帧大小**，不影响返回的字节总数，也不动"短读 = 到末尾"那条判据：
    // 对端（含 CFAPI 镜像水合那条路）一次 `len` 要多少还是拿多少。
    let stream = ReaderStream::with_capacity(limited, crate::net::STREAM_CHUNK);
    Ok(Body::from_stream(stream))
}

/// `DELETE /entry?share=<id>&path=<relative_path>` —— 删除共享项里的一个**文件或目录**。
///
/// 文件直接删，目录递归删；共享项根一律拒绝。根是用户整项共享的落点，误点一次会丢掉
/// 整棵树，根级操作仍只由共享项设备的界面负责。
///
/// 删除前与 `/dav` 一样先登记短时归属：watcher 在 300ms 内看到 `deleted`，命中归属表
/// 才会把这条更新记录写成**调用方设备**，否则会误记成共享项设备自己。
pub async fn delete_entry_handler(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<StatusCode, (StatusCode, String)> {
    let share_id = params.get("share").ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "Missing 'share' query parameter".into(),
        )
    })?;
    let rel_path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let rel_path = crate::server::normalized_share_path(rel_path);
    if rel_path.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "不能删除共享项根".into()));
    }
    if !crate::server::safe_relative_share_path(&rel_path) {
        return Err((StatusCode::FORBIDDEN, "Path access denied".into()));
    }

    // 不可见与不存在都回 404：不向对端泄露「本机有这么个共享项」。
    let share_root = {
        let config = state.config.lock();
        config
            .shares
            .iter()
            .find(|s| s.id == *share_id && caller.sees_share(&state, s))
            .map(|s| s.path.clone())
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    format!("Share '{share_id}' not found"),
                )
            })?
    };

    let root = tokio::fs::canonicalize(&share_root).await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法解析共享项根目录".to_string(),
        )
    })?;
    let target = root.join(&rel_path);
    let canonical = tokio::fs::canonicalize(&target)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "路径不存在".to_string()))?;
    if !canonical.starts_with(&root) {
        return Err((StatusCode::FORBIDDEN, "Path traversal denied".into()));
    }
    // 检查/删除的是**这一条路径本身**（符号链接删链接，不删目标）；canonicalize 只做越界检查。
    let meta = tokio::fs::symlink_metadata(&target)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("读取元数据失败: {e}")))?;
    let is_dir = meta.is_dir();

    crate::server::routes_dav::attribute_peer(&state, &caller, share_id, &rel_path);
    let removed = if is_dir {
        tokio::fs::remove_dir_all(&target).await
    } else {
        tokio::fs::remove_file(&target).await
    };
    removed.map_err(|e| (StatusCode::FORBIDDEN, format!("删除失败: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ShareItem;

    /// 共享项根**就是一个文件**时，空 `path` 必须当"根自身"。
    ///
    /// 回归的是这条链：`share_root.join("")` 会带上结尾分隔符（`F:\x\a.txt\`），
    /// `canonicalize` 对文件报 `ERROR_INVALID_NAME`(os 267) → 本 handler 回 403。
    /// 拖拽下载拖的正是"共享项根就是一个文件"这一类（`path` 为空），于是对端表现为
    /// 起拖成功、落下报"未指定的错误"、文件始终不出现。
    #[tokio::test]
    async fn file_root_share_is_served_with_an_empty_path() {
        let dir = std::env::temp_dir().join(format!("orbdrop_file_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("single.txt");
        std::fs::write(&file, b"hello from a file share").unwrap();

        let mut config = crate::config::test_config();
        let share = ShareItem::new(file.clone()); // 共享项根指向一个文件
        let share_id = share.id.clone();
        config.shares.push(share);
        let config_path =
            std::env::temp_dir().join(format!("orbdrop_file_cfg_{}.toml", uuid::Uuid::new_v4()));
        let state = Arc::new(AppState::new(config, config_path));
        let port = crate::server::start_server_loopback(state.clone(), 4070)
            .await
            .expect("起服务");

        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/file"))
            .query(&[("share", share_id.as_str()), ("path", ""), ("offset", "0")])
            .send()
            .await
            .expect("请求");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "空 path 要落到共享项根自身，不能 403"
        );
        assert_eq!(
            resp.bytes().await.unwrap().as_ref(),
            b"hello from a file share"
        );

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「删除」支持文件与目录（目录递归），但共享项根（含 `./` 归一后的根）与越界路径
    /// 一律拒绝，且被拒时磁盘上的东西一个都不动。
    #[tokio::test]
    async fn delete_entry_removes_files_and_directories_inside_the_share() {
        let dir = std::env::temp_dir().join(format!("orbdrop_delete_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("hello.txt"), b"hello").unwrap();
        std::fs::write(dir.join("sub").join("inner.txt"), b"inner").unwrap();

        let mut config = crate::config::test_config();
        let share = ShareItem::new(dir.clone());
        let share_id = share.id.clone();
        config.shares.push(share);
        let config_path =
            std::env::temp_dir().join(format!("orbdrop_delete_cfg_{}.toml", uuid::Uuid::new_v4()));
        let state = Arc::new(AppState::new(config, config_path));
        let port = crate::server::start_server_loopback(state.clone(), 4095)
            .await
            .expect("起服务");
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/entry");

        let resp = client
            .delete(url.as_str())
            .query(&[("share", share_id.as_str()), ("path", "hello.txt")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
        assert!(!dir.join("hello.txt").exists(), "文件应该真的被删掉");

        // 目录：整棵子树递归删除。
        let resp = client
            .delete(url.as_str())
            .query(&[("share", share_id.as_str()), ("path", "sub")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
        assert!(!dir.join("sub").exists(), "目录应该真的被删掉");

        // 共享项根：空 path 与 `./` 都不能落到根自身。
        for path in ["", "./"] {
            let resp = client
                .delete(url.as_str())
                .query(&[("share", share_id.as_str()), ("path", path)])
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                reqwest::StatusCode::BAD_REQUEST,
                "path={path:?} 不该删共享项根"
            );
        }
        assert!(dir.exists(), "共享项根目录必须还在");

        let resp = client
            .delete(url.as_str())
            .query(&[("share", share_id.as_str()), ("path", "../outside.txt")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
