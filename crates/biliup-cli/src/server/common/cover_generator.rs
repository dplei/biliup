use crate::server::errors::{AppError, AppResult};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::{Rgb, RgbImage, codecs::jpeg::JpegEncoder};
use imageproc::drawing::{draw_text_mut, text_size};

/// 内嵌主字体（思源黑体 CN Bold）：覆盖中文/英文/数字。
const FONT_BYTES: &[u8] = include_bytes!("../../../assets/fonts/SourceHanSansCN-Bold.otf");
/// 内嵌回退字体（Noto Emoji，单色轮廓版，OFL 协议）：主字体缺失的 emoji 等字形改用它。
/// 注意：当前渲染栈（ab_glyph）只能光栅化单色轮廓，emoji 呈白色单色剪影，并非彩色。
const EMOJI_FONT_BYTES: &[u8] = include_bytes!("../../../assets/fonts/NotoEmoji-Regular.ttf");

/// 把封面文字模板（已完成占位符/时间替换）切分成多行。
/// 模板里用字面量 `\n`（反斜杠 + n）表示换行——前端是单行输入框，不可能有真实换行符，
/// 所以先把字面 `\n` 还原成真实换行符，再按真实换行符切分；同时兼容真实换行符。
pub fn split_template_lines(text: &str) -> Vec<String> {
    text.replace("\\n", "\n")
        .split('\n')
        .map(|s| s.to_string())
        .collect()
}

/// 为单个字符挑选字体：主字体有该字形则用主字体，否则按顺序回退。
/// 都没有时返回 0（主字体），让其画出 .notdef 占位，避免吞字。
fn font_index_for_char(fonts: &[FontRef], c: char) -> usize {
    fonts.iter().position(|f| f.glyph_id(c).0 != 0).unwrap_or(0)
}

/// 把一行文字按字体切成连续片段：相邻、同字体的字符合并为一段。
fn segment_runs(fonts: &[FontRef], line: &str) -> Vec<(usize, String)> {
    let mut runs: Vec<(usize, String)> = Vec::new();
    for c in line.chars() {
        let fi = font_index_for_char(fonts, c);
        match runs.last_mut() {
            Some((last_fi, s)) if *last_fi == fi => s.push(c),
            _ => runs.push((fi, c.to_string())),
        }
    }
    runs
}

/// 计算一行在多字体混排下的总宽度（各片段宽度之和）。
fn measure_line(fonts: &[FontRef], scale: PxScale, line: &str) -> f32 {
    segment_runs(fonts, line)
        .iter()
        .map(|(fi, s)| text_size(scale, &fonts[*fi], s).0 as f32)
        .sum()
}

/// 封面背景。本版仅实现 Black；下一版加 Image(PathBuf)。
#[derive(Debug, Clone)]
pub enum Background {
    Black,
}

/// 封面渲染选项
#[derive(Debug, Clone)]
pub struct CoverOptions {
    pub width: u32,
    pub height: u32,
    pub background: Background,
    /// 最大字号（按行宽自适应时的上限，单位 px）
    pub base_font_px: f32,
    /// 四周安全边距（px）
    pub margin: u32,
    pub text_color: Rgb<u8>,
    pub stroke_color: Rgb<u8>,
    /// 行间距系数（相对单行高度）
    pub line_gap_ratio: f32,
}

impl Default for CoverOptions {
    fn default() -> Self {
        Self {
            width: 1146,
            height: 717,
            background: Background::Black,
            base_font_px: 110.0,
            margin: 90,
            text_color: Rgb([255, 255, 255]),
            stroke_color: Rgb([0, 0, 0]),
            line_gap_ratio: 0.35,
        }
    }
}

/// 渲染封面，返回 JPG 字节。
/// lines: 已渲染好的多行文字（调用方负责占位符替换与按 \n 切分）。
pub fn render_cover(lines: &[String], opts: &CoverOptions) -> AppResult<Vec<u8>> {
    let primary = FontRef::try_from_slice(FONT_BYTES)
        .map_err(|e| AppError::Custom(format!("加载内嵌字体失败: {e}")))?;
    let emoji = FontRef::try_from_slice(EMOJI_FONT_BYTES)
        .map_err(|e| AppError::Custom(format!("加载内嵌 emoji 字体失败: {e}")))?;
    // 顺序即回退优先级：主字体（中英文）→ emoji 字体。
    let fonts = [primary, emoji];

    // 1. 画布（背景）
    let bg = match opts.background {
        Background::Black => Rgb([0, 0, 0]),
    };
    let mut img = RgbImage::from_pixel(opts.width, opts.height, bg);

    let lines: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();

    // 2. 自适应字号：让最宽的一行不超过 (width - 2*margin)
    let max_text_w = opts.width.saturating_sub(2 * opts.margin) as f32;
    let mut scale_px = opts.base_font_px;
    let widest = lines
        .iter()
        .map(|l| measure_line(&fonts, PxScale::from(opts.base_font_px), l))
        .fold(0.0_f32, f32::max);
    if widest > max_text_w && widest > 0.0 {
        scale_px = opts.base_font_px * (max_text_w / widest);
    }
    let scale = PxScale::from(scale_px);

    // 3. 行高与垂直居中（行高以主字体为准）
    let scaled = fonts[0].as_scaled(scale);
    let line_h = scaled.height();
    let gap = line_h * opts.line_gap_ratio;
    // max(1): 避免空输入时除零，并让空白图仍垂直居中
    let n = lines.len().max(1) as f32;
    let total_h = n * line_h + (n - 1.0).max(0.0) * gap;
    let mut y = ((opts.height as f32 - total_h) / 2.0).max(opts.margin as f32);

    // 4. 逐行行内居中绘制（带轻微描边）
    for line in &lines {
        let lw = measure_line(&fonts, scale, line);
        let x = ((opts.width as f32 - lw) / 2.0).max(0.0);
        draw_line_with_stroke(&mut img, opts, &fonts, scale, x, y, line);
        y += line_h + gap;
    }

    // 5. 编码 JPG
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, 90)
        .encode_image(&img)
        .map_err(|e| AppError::Custom(format!("封面 JPG 编码失败: {e}")))?;
    Ok(buf)
}

/// 渲染并写入一个临时 JPG 文件，返回句柄（调用方持有以控制生命周期）
pub fn render_to_tempfile(
    lines: &[String],
    opts: &CoverOptions,
) -> AppResult<tempfile::NamedTempFile> {
    use std::io::Write;
    let bytes = render_cover(lines, opts)?;
    let mut f = tempfile::Builder::new()
        .prefix("biliup-cover-")
        .suffix(".jpg")
        .tempfile()
        .map_err(|e| AppError::Custom(format!("创建临时封面失败: {e}")))?;
    f.write_all(&bytes)
        .and_then(|_| f.flush())
        .map_err(|e| AppError::Custom(format!("写入临时封面失败: {e}")))?;
    Ok(f)
}

/// 画一行：先在 4 个对角偏移画描边色，再画正文，保证非黑背景上也清晰。
/// 4 次描边 pass 是有意为之——在未来非黑背景上提升可读性；当前黑底下视觉上无副作用。
/// 一行内可能混排多个字体（中英文 + emoji 回退），按片段顺序推进 x。
fn draw_line_with_stroke(
    img: &mut RgbImage,
    opts: &CoverOptions,
    fonts: &[FontRef],
    scale: PxScale,
    x: f32,
    y: f32,
    text: &str,
) {
    let runs = segment_runs(fonts, text);
    let off = 2_i32;
    for (dx, dy) in [(-off, -off), (off, -off), (-off, off), (off, off)] {
        draw_runs(img, opts.stroke_color, fonts, scale, x, y, dx, dy, &runs);
    }
    draw_runs(img, opts.text_color, fonts, scale, x, y, 0, 0, &runs);
}

/// 按片段（字体, 文字）顺序逐段绘制，每段用各自字体并累加宽度推进 x。
#[allow(clippy::too_many_arguments)]
fn draw_runs(
    img: &mut RgbImage,
    color: Rgb<u8>,
    fonts: &[FontRef],
    scale: PxScale,
    x: f32,
    y: f32,
    dx: i32,
    dy: i32,
    runs: &[(usize, String)],
) {
    let mut cur_x = x;
    for (fi, s) in runs {
        let font = &fonts[*fi];
        draw_text_mut(img, color, cur_x as i32 + dx, y as i32 + dy, scale, font, s);
        cur_x += text_size(scale, font, s).0 as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, ImageReader};
    use std::io::Cursor;

    fn decode_dims(bytes: &[u8]) -> (u32, u32) {
        let img = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        img.dimensions()
    }

    #[test]
    fn renders_jpeg_with_expected_dimensions() {
        let lines = vec!["测试主播".to_string(), "2026-06-13 12点场".to_string()];
        let bytes = render_cover(&lines, &CoverOptions::default()).unwrap();
        assert!(!bytes.is_empty(), "输出不应为空");
        assert_eq!(decode_dims(&bytes), (1146, 717));
    }

    #[test]
    fn handles_single_line() {
        let bytes = render_cover(&["只有一行".to_string()], &CoverOptions::default()).unwrap();
        assert_eq!(decode_dims(&bytes), (1146, 717));
    }

    #[test]
    fn handles_long_line_without_panic() {
        let long = "超长主播名".repeat(20);
        let bytes = render_cover(&[long], &CoverOptions::default()).unwrap();
        assert_eq!(decode_dims(&bytes), (1146, 717));
    }

    #[test]
    fn render_to_tempfile_writes_valid_jpeg() {
        let f = render_to_tempfile(&["主播".to_string()], &CoverOptions::default()).unwrap();
        let bytes = std::fs::read(f.path()).unwrap();
        assert_eq!(decode_dims(&bytes), (1146, 717));
    }

    // Bug 1：模板里的字面 `\n` 必须切成多行（单行输入框不会有真实换行符）。
    #[test]
    fn split_template_lines_splits_literal_backslash_n() {
        let lines = split_template_lines("cwd 🚥\\n2026-06-15");
        assert_eq!(lines, vec!["cwd 🚥".to_string(), "2026-06-15".to_string()]);
    }

    // 兼容真实换行符（以防来源含真实换行）。
    #[test]
    fn split_template_lines_splits_real_newline() {
        let lines = split_template_lines("a\nb");
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
    }

    // 无换行标记时返回单行。
    #[test]
    fn split_template_lines_single_line() {
        assert_eq!(
            split_template_lines("只有一行"),
            vec!["只有一行".to_string()]
        );
    }

    // Bug 2：思源黑体没有的 emoji（如 🚥）应回退到内嵌 emoji 字体，而非主字体的 .notdef。
    #[test]
    fn emoji_falls_back_to_emoji_font() {
        let primary = FontRef::try_from_slice(FONT_BYTES).unwrap();
        let emoji = FontRef::try_from_slice(EMOJI_FONT_BYTES).unwrap();
        let fonts = [primary, emoji];
        // 中文走主字体（index 0）
        assert_eq!(font_index_for_char(&fonts, '测'), 0);
        // 🚥 走 emoji 字体（index 1）：主字体无此字形，emoji 字体有
        assert!(
            fonts[0].glyph_id('🚥').0 == 0,
            "主字体本不应含 🚥 字形（否则该用例失去意义）"
        );
        assert_eq!(font_index_for_char(&fonts, '🚥'), 1);
    }

    // 混排一行应切成「中文段 + emoji 段 + 中文段」三段，且分属不同字体。
    #[test]
    fn segment_runs_splits_by_font() {
        let primary = FontRef::try_from_slice(FONT_BYTES).unwrap();
        let emoji = FontRef::try_from_slice(EMOJI_FONT_BYTES).unwrap();
        let fonts = [primary, emoji];
        let runs = segment_runs(&fonts, "cwd🚥前");
        assert_eq!(runs.len(), 3, "应切成 3 段，实际: {runs:?}");
        assert_eq!(runs[0], (0, "cwd".to_string()));
        assert_eq!(runs[1], (1, "🚥".to_string()));
        assert_eq!(runs[2], (0, "前".to_string()));
    }

    // 含 emoji 的封面应正常渲染出合法 JPEG，不 panic、不吞字。
    #[test]
    fn renders_cover_with_emoji() {
        let lines = vec!["cwd 🚥".to_string(), "2026-06-15".to_string()];
        let bytes = render_cover(&lines, &CoverOptions::default()).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(decode_dims(&bytes), (1146, 717));
    }
}
