use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use axum::extract::{Query, WebSocketUpgrade};
use serde::Deserialize;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, info};

static ALLOWED_FILES: &[&str] = &["ds_update.log", "download.log", "upload.log"];

#[derive(Debug, Deserialize, Clone)]
pub struct LogsQuery {
    file: Option<String>,
}

pub async fn ws_logs(
    ws: WebSocketUpgrade,
    Query(query): Query<LogsQuery>,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| websocket_logs(socket, query))
}

async fn websocket_logs(mut ws: WebSocket, query: LogsQuery) {
    // 参数获取与校验
    let file_param = query.file.unwrap_or_else(|| "ds_update.log".to_string());
    if !ALLOWED_FILES.contains(&file_param.as_str()) {
        let _ = ws
            .send(Message::Text(
                format!("不允许访问请求的文件: {}", file_param).into(),
            ))
            .await;
        let _ = ws.send(Message::Close(None)).await;
        return;
    }

    // 日志按天滚动后真实文件名是 ds_update.<日期>.log，这里把逻辑名解析成最新的实体文件。
    let mut log_file = match resolve_latest_log(&file_param) {
        Some(p) => p,
        None => {
            let _ = ws
                .send(Message::Text(
                    format!("日志文件 {} 不存在", file_param).into(),
                ))
                .await;
            let _ = ws.send(Message::Close(None)).await;
            return;
        }
    };

    // 发送初始内容（最后50行）并获取当前大小
    let mut file_size = match send_last_lines(&mut ws, &log_file, 50).await {
        Ok(size) => size,
        Err(e) => {
            match e.kind() {
                ErrorKind::NotFound => {
                    let _ = ws
                        .send(Message::Text(
                            format!("日志文件 {} 不存在", log_file.display()).into(),
                        ))
                        .await;
                }
                _ => {
                    let _ = ws
                        .send(Message::Text(format!("读取日志文件错误: {}", e).into()))
                        .await;
                    error!("读取日志文件错误: {}", e);
                }
            }
            let _ = ws.send(Message::Close(None)).await;
            return;
        }
    };

    // 心跳/轮询间隔
    let mut tick = interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // 保活：日志安静期连接上零流量，前置代理(NPM/nginx，默认 proxy_read_timeout ~60s)会按
    // 空闲超时把 WS 重置（表现为「Connection reset without closing handshake」）。每 30s 主动
    // 发一个 Ping 制造流量，避免空闲期被掐断。
    let mut ping_tick = interval(Duration::from_secs(30));
    ping_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // 首拍立即触发，跳过以免一上来就发 Ping
    ping_tick.tick().await;

    // 主循环：同时处理客户端消息和文件更新
    loop {
        tokio::select! {
            maybe_msg = ws.recv() => {
                match maybe_msg {
                    Some(Ok(Message::Close(_))) => {
                        let _ = ws.send(Message::Close(None)).await;
                        break;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        // 回应 PONG
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(_)) => {
                        // 其他消息不处理（Text/Binary等）
                    }
                    Some(Err(e)) => {
                        // 客户端/代理无握手直接重置（如空闲超时、关页、刷新）是常见的良性断连，
                        // 不是服务端错误，降为 debug，避免日志刷红。
                        debug!("WebSocket连接断开: {}", e);
                        break;
                    }
                    None => {
                        info!("WebSocket连接已关闭");
                        break;
                    }
                }
            }

            _ = ping_tick.tick() => {
                // 空闲期主动发 Ping 保活；发送失败说明连接已断，退出
                if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                    debug!("WebSocket保活Ping发送失败，连接已断开");
                    break;
                }
            }

            _ = tick.tick() => {
                // 跨天滚动：重新解析最新日志文件，若切换了就重载最后几行
                if let Some(latest) = resolve_latest_log(&file_param)
                    && latest != log_file
                {
                    log_file = latest;
                    let _ = ws
                        .send(Message::Text("日志已按天滚动到新文件，重新加载...".into()))
                        .await;
                    match send_last_lines(&mut ws, &log_file, 50).await {
                        Ok(size) => {
                            file_size = size;
                            continue;
                        }
                        Err(e) => {
                            let _ = ws
                                .send(Message::Text(format!("读取日志文件错误: {}", e).into()))
                                .await;
                            error!("读取日志文件错误: {}", e);
                            break;
                        }
                    }
                }
                // 文件是否存在
                let meta = match fs::metadata(&log_file).await {
                    Ok(m) => m,
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        let _ = ws.send(Message::Text(format!(
                            "日志文件 {} 不再存在",
                            log_file.display()
                        ).into())).await;
                        break;
                    }
                    Err(e) => {
                        let _ = ws.send(Message::Text(format!("监控日志文件错误: {}", e).into())).await;
                        error!("websocket_logs错误: {}", e);
                        break;
                    }
                };

                let current_size = meta.len();

                // 文件被截断
                if current_size < file_size {
                    let _ = ws.send(Message::Text(Utf8Bytes::from("日志文件被截断，重新加载...".to_string()))).await;
                    match send_last_lines(&mut ws, &log_file, 50).await {
                        Ok(size) => file_size = size,
                        Err(e) => {
                            let _ = ws.send(Message::Text(format!("读取日志文件错误: {}", e).into())).await;
                            error!("读取日志文件错误: {}", e);
                            break;
                        }
                    }
                    continue;
                }

                // 文件新增内容
                if current_size > file_size {
                    if let Err(e) = send_new_lines_from_offset(&mut ws, &log_file, file_size).await {
                        let _ = ws.send(Message::Text(format!("监控日志文件错误: {}", e).into())).await;
                        error!("websocket_logs错误: {}", e);
                        break;
                    }
                    file_size = current_size;
                }
            }
        }
    }

    let _ = ws.send(Message::Close(None)).await;
    debug!("WebSocket日志会话结束: {}", file_param);
}

/// 把逻辑日志名（如 "ds_update.log"）解析为工作目录下实际最新的日志文件。
/// 按天滚动后真实文件名形如 "ds_update.2026-06-12.log"，这里匹配「精确同名」或
/// 「<prefix>.*.<suffix>」两种，取修改时间最新的一个；都不存在则返回 None。
fn resolve_latest_log(file_param: &str) -> Option<PathBuf> {
    let (prefix, suffix) = file_param.rsplit_once('.').unwrap_or((file_param, ""));
    let dot_prefix = format!("{prefix}.");
    let dot_suffix = format!(".{suffix}");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(".").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let matches =
            name == file_param || (name.starts_with(&dot_prefix) && name.ends_with(&dot_suffix));
        if !matches {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if newest.as_ref().map(|(t, _)| mtime >= *t).unwrap_or(true) {
            newest = Some((mtime, entry.path()));
        }
    }
    newest.map(|(_, p)| p)
}

// 发送最后 n 行，并返回当前文件大小
async fn send_last_lines(
    ws: &mut WebSocket,
    path: &std::path::Path,
    n: usize,
) -> std::io::Result<u64> {
    let meta = fs::metadata(path).await?;
    let file_size = meta.len();

    let file = fs::File::open(path).await?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    let mut buf: VecDeque<String> = VecDeque::with_capacity(n);
    while let Some(line) = lines.next_line().await? {
        if buf.len() == n {
            buf.pop_front();
        }
        buf.push_back(line);
    }
    for line in buf {
        ws.send(Message::Text(Utf8Bytes::from(line)))
            .await
            .map_err(|e| {
                std::io::Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("发送WebSocket消息失败: {}", e),
                )
            })?;
    }
    Ok(file_size)
}

// 从偏移量开始读取新增内容，并逐行发送
async fn send_new_lines_from_offset(
    ws: &mut WebSocket,
    path: &std::path::Path,
    offset: u64,
) -> std::io::Result<()> {
    let mut file = fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;

    // 直接读到字符串（UTF-8），若遇到非UTF-8可换成读bytes+lossy
    let mut s = String::new();
    if let Err(e) = file.read_to_string(&mut s).await {
        // 如果遇到非UTF-8数据，降级为 lossy
        let mut bytes = Vec::new();
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read_to_end(&mut bytes).await?;
        s = String::from_utf8_lossy(&bytes).into_owned();
        if e.kind() != ErrorKind::InvalidData {
            // 非编码错误也要汇报
            error!("读取日志文件新内容失败: {}", e);
        }
    }

    for line in s.lines() {
        ws.send(Message::Text(Utf8Bytes::from(line.to_string())))
            .await
            .map_err(|e| {
                std::io::Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("发送WebSocket消息失败: {}", e),
                )
            })?;
    }
    Ok(())
}
