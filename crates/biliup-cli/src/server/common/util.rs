use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::models::StreamerInfo;
use crate::server::infrastructure::models::hook_step::HookStep;
use chrono::format::{Item, StrftimeItems};
use chrono::{Duration, Local};
use error_stack::{ResultExt, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{error, info};
use url::Url;

/// 录制器配置结构体
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Recorder {
    ///  filename_prefix   文件名前缀模板
    pub filename_prefix: Option<String>,
    /// 直播间信息
    pub streamer_info: StreamerInfo,
}

impl Recorder {
    pub fn new(filename_prefix: Option<String>, streamer_info: StreamerInfo) -> Self {
        Self {
            filename_prefix,
            streamer_info,
        }
    }

    /// 生成文件名模板（包含时间格式占位符），并清洗非法字符
    pub fn filename_template(&self) -> String {
        if let Some(prefix) = &self.filename_prefix {
            render_filename_template(
                prefix,
                &self.streamer_info.name,
                &self.streamer_info.title,
                &self.streamer_info.url,
            )
        } else {
            let streamer = sanitize_filename_chars(&self.streamer_info.name);
            finish_filename_sanitization(format!("{streamer}%Y-%m-%dT%H_%M_%S"))
        }
    }

    fn template_with(&self, template: &str) -> String {
        template
            .replace("{streamer}", &self.streamer_info.name)
            .replace("{title}", &self.streamer_info.title)
            .replace("{url}", &self.streamer_info.url)
    }

    /// 生成“基名”（不带扩展名），时间冲突时按秒+1继续尝试，直到唯一
    pub fn generate_filename(&self, suffix: &str) -> String {
        let template = self.filename_template();
        let mut t = Local::now();

        loop {
            let base = t.format(&template).to_string();
            if !self.exists_with_suffix(&base, suffix) {
                return base;
            }
            t += Duration::seconds(1);
        }
    }

    /// 生成“基名”（不带扩展名）
    pub fn format_filename(&self) -> String {
        let template = self.filename_template();
        self.streamer_info
            .date
            .with_timezone(&Local)
            .format(&template)
            .to_string()
    }

    /// 展开 `{streamer}` 等占位符与时间格式；时间格式串非法时返回 None。
    ///
    /// 独立于 `format` 存在，是因为 chrono 对非法格式串（例如落单的 `%`）不是报错而是
    /// **panic**：`DelayedFormat` 的 Display 返回 Err，`to_string()` 直接炸。模板一旦来自
    /// 用户当场输入——封面预览接口正是如此——手滑一个 `%` 就能把请求打崩。
    ///
    /// 校验的是**替换之后**的串：主播名、直播标题里都可能带 `%`，只验模板原文会漏。
    pub fn try_format(&self, template: &str) -> Option<String> {
        let substituted = self.template_with(template);

        if StrftimeItems::new(&substituted).any(|item| matches!(item, Item::Error)) {
            return None;
        }

        Some(
            self.streamer_info
                .date
                .with_timezone(&Local)
                .format(&substituted)
                .to_string(),
        )
    }

    /// 非法格式串退化成「占位符原样保留」而非 panic：封面只是稿件的附属品，
    /// 不值得让一整场录播的提交因为模板里一个 `%` 而炸掉整个上传任务。
    pub fn format(&self, template: &str) -> String {
        self.try_format(template).unwrap_or_else(|| {
            error!(template, "时间格式串不合法，占位符按原样保留");
            self.template_with(template)
        })
    }

    /// 直接生成带扩展名的完整路径（当前目录下）
    pub fn generate_path(&self, suffix: &str) -> PathBuf {
        PathBuf::from(self.generate_filename(suffix)).with_extension(suffix)
    }

    fn exists_with_suffix(&self, base: &str, suffix: &str) -> bool {
        Path::new(base).with_extension(&suffix).exists()
    }
}

/// 非法字符清洗（最小可用实现）
/// - 替换常见非法字符为 '_'；去掉末尾空格与点
/// - 普通 ':' 会被替换为 '_'；需要字面冒号时在文件名模板中写作 `{colon}`
/// - 保留 '%'，以便 strftime 能正常工作
fn sanitize_filename_chars(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => out.push('_'),
            _ => out.push(ch),
        }
    }
    out
}

fn finish_filename_sanitization(name: String) -> String {
    let out = name.trim_end_matches([' ', '.']).to_string();
    if out.is_empty() { "_".to_string() } else { out }
}

fn render_filename_template(template: &str, streamer: &str, title: &str, url: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        out.push_str(&sanitize_filename_chars(&rest[..start]));
        rest = &rest[start..];

        if rest.starts_with("{streamer}") {
            out.push_str(&sanitize_filename_chars(streamer));
            rest = &rest["{streamer}".len()..];
        } else if rest.starts_with("{title}") {
            out.push_str(&sanitize_filename_chars(title));
            rest = &rest["{title}".len()..];
        } else if rest.starts_with("{url}") {
            out.push_str(&sanitize_filename_chars(url));
            rest = &rest["{url}".len()..];
        } else if rest.starts_with("{colon}") {
            out.push(':');
            rest = &rest["{colon}".len()..];
        } else {
            out.push('{');
            rest = &rest['{'.len_utf8()..];
        }
    }

    out.push_str(&sanitize_filename_chars(rest));
    finish_filename_sanitization(out)
}

/// 生成弹幕文件名模板（包含时间格式占位符），并清洗非法字符
pub fn danmaku_filename_template(filename_prefix: Option<&str>, name: &str) -> String {
    filename_prefix
        .map(|prefix| render_filename_template(prefix, name, "", ""))
        .unwrap_or_else(|| {
            let streamer = sanitize_filename_chars(name);
            finish_filename_sanitization(format!("{streamer}%Y-%m-%dT%H_%M_%S"))
        })
}

/// 从 URL 中提取媒体扩展名（小写），例如 "flv", "mp4" 等。
/// 先尝试解析 URL 的 path 的扩展名；如果没有，再查 query 中常见的参数（format/type/ext）。
/// 返回 None 表示无法判断。
pub fn media_ext_from_url(input: &str) -> Option<String> {
    // 统一的扩展名清洗：去空白/前导点，切掉 MIME/分隔符，并转小写
    fn clean_ext(val: &str) -> Option<String> {
        let token = val
            .trim()
            .trim_start_matches('.') // .mp4 -> mp4
            .split(['/', ';', ',', '?', '&', '#']) // video/mp4;codecs=...
            .next()
            .map(str::trim)
            .unwrap_or("");
        if token.is_empty() {
            None
        } else {
            Some(token.to_ascii_lowercase())
        }
    }

    // 1) 先尝试按 URL 解析
    if let Ok(url) = Url::parse(input) {
        // a) 从最后一个 path segment 取扩展名
        if let Some(seg) = url.path_segments().and_then(|mut s| s.next_back())
            && let Some((_, ext)) = seg.rsplit_once('.')
            && let Some(ext) = clean_ext(ext)
        {
            return Some(ext);
        }

        // b) 常见 query 参数中找一次（不重复多轮扫描），忽略大小写
        let keys = ["format", "type", "ext", "filetype", "fmt"];
        if let Some(ext) = url.query_pairs().find_map(|(k, v)| {
            if keys.iter().any(|t| k.as_ref().eq_ignore_ascii_case(t)) {
                clean_ext(&v)
            } else {
                None
            }
        }) {
            return Some(ext);
        }

        return None;
    }

    // 2) 不是完整 URL 的兜底：纯字符串/相对地址
    let before_q = input.split('?').next().unwrap_or(input);
    if let Some((_, ext)) = before_q.rsplit_once('.') {
        return clean_ext(ext);
    }

    None
}

pub fn parse_time(segment_time: &str) -> std::time::Duration {
    let parts: Vec<&str> = segment_time.split(':').collect();
    let h = parts[0].parse::<i32>().unwrap_or(1);
    let m = parts[1].parse::<i32>().unwrap_or(0);
    let s = parts[2].parse::<i32>().unwrap_or(0);
    std::time::Duration::from_secs((h * 3600 + m * 60 + s) as u64)
}

#[cfg(test)]
mod tests {
    use crate::server::common::util::{Recorder, media_ext_from_url};
    use crate::server::infrastructure::models::StreamerInfo;
    use chrono::{Local, TimeZone};

    #[test]
    fn filename_template_sanitizes_plain_colon_time_separators() {
        let start_time = Local
            .with_ymd_and_hms(2026, 6, 17, 14, 57, 18)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let recorder = Recorder::new(
            Some("{streamer} %Y-%m-%d %H:%M:%S".to_string()),
            StreamerInfo::new("小桐人", "https://example.com", "直播标题", start_time, ""),
        );

        assert_eq!(recorder.format_filename(), "小桐人 2026-06-17 14_57_18");
    }

    #[test]
    fn filename_template_preserves_colon_tokens_for_time_separators() {
        let start_time = Local
            .with_ymd_and_hms(2026, 6, 17, 14, 57, 18)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let recorder = Recorder::new(
            Some("{streamer} %Y-%m-%d %H{colon}%M{colon}%S".to_string()),
            StreamerInfo::new("小桐人", "https://example.com", "直播标题", start_time, ""),
        );

        assert_eq!(recorder.format_filename(), "小桐人 2026-06-17 14:57:18");
    }

    #[test]
    fn filename_template_sanitizes_unescaped_literal_colons() {
        let start_time = Local
            .with_ymd_and_hms(2026, 6, 17, 14, 57, 18)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let recorder = Recorder::new(
            Some("{streamer}: %Y-%m-%d %H{colon}%M{colon}%S".to_string()),
            StreamerInfo::new("小桐人", "https://example.com", "直播标题", start_time, ""),
        );

        assert_eq!(recorder.format_filename(), "小桐人_ 2026-06-17 14:57:18");
    }

    #[test]
    fn filename_template_does_not_treat_streamer_text_as_colon_token() {
        let start_time = Local
            .with_ymd_and_hms(2026, 6, 17, 14, 57, 18)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let recorder = Recorder::new(
            Some("{streamer} %H{colon}%M{colon}%S".to_string()),
            StreamerInfo::new(
                "小桐人{colon}:备注",
                "https://example.com",
                "直播标题",
                start_time,
                "",
            ),
        );

        assert_eq!(recorder.format_filename(), "小桐人{colon}_备注 14:57:18");
    }

    #[test]
    fn danmaku_filename_template_uses_colon_token() {
        assert_eq!(
            crate::server::common::util::danmaku_filename_template(
                Some("{streamer} %H{colon}%M{colon}%S"),
                "小桐人:备注",
            ),
            "小桐人_备注 %H:%M:%S"
        );
    }

    /// 固定时刻的录制器，供下面几条格式化用例共用。
    fn recorder_at(template: Option<&str>) -> Recorder {
        let date = Local
            .with_ymd_and_hms(2026, 7, 23, 20, 5, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        Recorder::new(
            template.map(str::to_string),
            StreamerInfo::new("小桐人", "https://example.com", "直播标题", date, ""),
        )
    }

    #[test]
    fn try_format_expands_placeholders_and_time() {
        let recorder = recorder_at(None);

        assert_eq!(
            recorder.try_format("{streamer}\\n%Y-%m-%d %H点场"),
            Some("小桐人\\n2026-07-23 20点场".to_string())
        );
    }

    /// 落单的 `%` 不是合法的时间格式说明符。
    #[test]
    fn try_format_rejects_dangling_percent() {
        assert_eq!(recorder_at(None).try_format("主播 %"), None);
    }

    /// 这条同时证明了 `try_format` 为什么必须存在：chrono 对非法格式串是 **panic**
    /// 而非报错，`format` 若直接调 `to_string()`，本用例会当场炸掉而不是失败。
    /// 现在它退化成「占位符原样保留」，投稿流程不会被模板里一个 `%` 拖垮。
    #[test]
    fn format_degrades_instead_of_panicking_on_invalid_specifier() {
        assert_eq!(recorder_at(None).format("主播 %"), "主播 %");
    }

    /// 主播名里带 `%` 时，校验必须发生在**替换之后**——只验模板原文会漏掉它。
    #[test]
    fn try_format_validates_after_substitution() {
        let date = Local
            .with_ymd_and_hms(2026, 7, 23, 20, 5, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let recorder = Recorder::new(
            None,
            StreamerInfo::new("100%", "https://example.com", "直播标题", date, ""),
        );

        assert_eq!(recorder.try_format("{streamer}"), None);
    }

    #[test]
    fn it_works() {
        assert_eq!(
            media_ext_from_url(
                "https://hwa.douyucdn2.cn/live/6512r9pAbb5Ercd1.flv?wsAuth=c77de01c8fcbc7b04b3d6daf66e523f5&token=web-h5-0-6512-f52253ea808109b3e2b66f385345c5e4ebdd692a847af73b&logo=0&expire=0&did=b6b79db91ca484562dcd6a1d5cdd9639&ver=219032101&pt=2&st=0&sid=420338944&mcid2=0&origin=dy&fcdn=hw&fo=0&mix=0&isp="
            ),
            Some("flv".to_string())
        );
    }
}

/// 文件验证配置
#[derive(Clone)]
pub struct FileValidator {
    min_size: u64,
    check_format: bool,
}

impl FileValidator {
    pub fn new(min_size: u64, check_format: bool) -> Self {
        Self {
            min_size,
            check_format,
        }
    }
}

impl Default for FileValidator {
    fn default() -> Self {
        Self {
            min_size: 1024 * 1024 * 100, // 100MB minimum
            check_format: true,
        }
    }
}

impl FileValidator {
    /// 验证文件有效性
    pub fn validate(&self, path: &Path) -> AppResult<()> {
        let metadata = fs::metadata(path).change_context(AppError::Unknown)?;

        let size = metadata.len();

        if size < self.min_size {
            let display = path.display();
            let path = path.to_owned();
            tokio::spawn(async move {
                let Ok(()) = HookStep::remove_file(&[&path])
                    .await
                    .inspect_err(|e| error!(e=?e))
                else {
                    return;
                };
                info!("过滤删除 - {}", path.display());
            });
            bail!(AppError::Custom(format!(
                "File {display} too small: {size} bytes, minimum: {} bytes",
                self.min_size
            )));
        }

        // 可选：检查文件格式
        if self.check_format {
            self.validate_format(path)?;
        }

        Ok(())
    }

    fn validate_format(&self, path: &Path) -> AppResult<()> {
        // 简单的格式验证 - 检查扩展名
        if let Some(extension) = path.extension() {
            let ext = extension.to_string_lossy().to_lowercase();
            match ext.as_str() {
                "mp4" | "flv" | "ts" | "m3u8" | "mkv" => Ok(()),
                _ => bail!(AppError::Custom(format!("Unsupported format: {}", ext))),
            }
        } else {
            bail!(AppError::Custom("No file extension found".to_string()))
        }
    }
}
