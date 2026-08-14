use axum::http::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::str::FromStr;

/// 平台 cookie 健康监测（检测失效并经横幅/webhook 提示）
pub mod cookie_health;
pub mod cover_generator;
pub mod download;
pub mod missing_segment;
/// 用户提供路径的安全解析（静态文件接口与后续的图片上传共用）
pub mod path_safety;
pub mod route_health;
/// 上传前时间戳异常检测与修复
pub mod timestamp_repair;
pub mod upload;
pub mod upload_session;
/// 通用工具函数
pub mod util;

pub fn construct_headers(hash_map: &HashMap<String, String>) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for (key, value) in hash_map.iter() {
        let name =
            HeaderName::from_str(key).map_err(|e| format!("invalid header name {key:?}: {e}"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|e| format!("invalid header value for {key:?}: {e}"))?;
        headers.insert(name, value);
    }
    Ok(headers)
}
