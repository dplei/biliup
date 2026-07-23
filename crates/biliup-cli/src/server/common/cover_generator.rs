use crate::server::errors::{AppError, AppResult};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::imageops::FilterType;
use image::{Rgb, RgbImage, codecs::jpeg::JpegEncoder};
use imageproc::drawing::{draw_text_mut, text_size};
use std::path::{Path, PathBuf};
use tracing::error;

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

/// 封面背景。
#[derive(Debug, Clone)]
pub enum Background {
    /// 纯黑（默认）
    Black,
    /// 本地图片文件。读取或解码失败时退回 Black，不让一张坏图毁掉整场录播的提交。
    Image(PathBuf),
    /// 已在内存中备好、且已适配画布尺寸的位图。
    ///
    /// 给本地调参用：预览子命令要在适配之后、画字之前叠加压暗与模糊，
    /// 有了这个变体就不必把中间结果存成临时文件再让渲染器重新解码一遍
    /// （那样会白白多一次有损压缩，本地看到的就不再等于线上产出）。
    /// 尺寸不符时按坏图处理，回退纯黑。
    Bitmap(RgbImage),
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

/// 铺好画布背景。
///
/// 图片背景出任何问题都退回纯黑：封面只是稿件的附属品，不值得让一整场录播的提交
/// 因为一张坏图而失败——与「自动封面生成失败则回退 cover_path」的处理态度一致。
fn render_background(opts: &CoverOptions) -> RgbImage {
    let black = || RgbImage::from_pixel(opts.width, opts.height, Rgb([0, 0, 0]));

    match &opts.background {
        Background::Black => black(),
        Background::Image(path) => match load_fitted(path, opts.width, opts.height) {
            Ok(img) => img,
            Err(e) => {
                error!(path = ?path, error = %e, "读取封面背景图失败，回退为纯黑背景");
                black()
            }
        },
        Background::Bitmap(img) if img.dimensions() == (opts.width, opts.height) => img.clone(),
        Background::Bitmap(img) => {
            error!(
                got = ?img.dimensions(),
                want = ?(opts.width, opts.height),
                "背景位图尺寸与画布不符，回退为纯黑背景"
            );
            black()
        }
    }
}

/// 图片尺寸不适合当封面背景（空图，或极端宽高比导致缩放后大到无法处理）。
///
/// 借用 image 的错误类型是为了让 `load_fitted` 的返回类型保持单一；`UnsupportedError`
/// 携带自定义描述，日志里不会被误读成解码失败。
fn unsupported_dimensions() -> image::ImageError {
    use image::error::{ImageFormatHint, UnsupportedError, UnsupportedErrorKind};
    image::ImageError::Unsupported(UnsupportedError::from_format_and_kind(
        ImageFormatHint::Unknown,
        UnsupportedErrorKind::GenericFeature("背景图尺寸不可用（空图或宽高比过于极端）".into()),
    ))
}

/// 按 cover 语义把图片适配到画布：等比缩放到刚好覆盖，再居中裁剪掉溢出部分。
/// 不拉伸变形、不留边——与 CSS 的 `object-fit: cover` 同义。
///
/// 公开是为了让本地调参的封面预览子命令共用同一套适配规则：它需要先拿到适配后的图，
/// 再叠加压暗与模糊。规则只此一份，本地看到的构图就是线上的构图。
pub fn load_fitted(path: &Path, width: u32, height: u32) -> Result<RgbImage, image::ImageError> {
    let src = image::open(path)?.to_rgb8();
    let (sw, sh) = (src.width(), src.height());

    // 空图无法参与比例运算，按尺寸不可用处理（交由调用方回退纯黑）
    if sw == 0 || sh == 0 {
        return Err(unsupported_dimensions());
    }

    // 取较大的缩放比，保证两个方向都被覆盖满
    let scale = f64::max(width as f64 / sw as f64, height as f64 / sh as f64);
    // ceil 而非 round：避免浮点误差导致缩放结果比画布小 1px，裁剪时越界
    let (rw, rh) = (
        ((sw as f64 * scale).ceil() as u32).max(width),
        ((sh as f64 * scale).ceil() as u32).max(height),
    );

    // 极端宽高比（例如 1×3000）会让另一边被放大成天文数字：1146×3440000 需要约 11.8 GB，
    // 分配失败会 panic 并穿透 render_cover，把「坏图回退纯黑」的保证一并击穿。
    // 这里提前拦下，按解码失败处理。
    const MAX_RESIZE_PIXELS: u64 = 64 << 20; // 6400 万像素，远超任何正常封面素材
    if rw as u64 * rh as u64 > MAX_RESIZE_PIXELS {
        return Err(unsupported_dimensions());
    }

    let mut resized = image::imageops::resize(&src, rw, rh, FilterType::Lanczos3);
    let (x, y) = ((rw - width) / 2, (rh - height) / 2);

    Ok(image::imageops::crop(&mut resized, x, y, width, height).to_image())
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
    let mut img = render_background(opts);

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

/// 测试专用工具，供本模块与预览子命令的测试共用。
#[cfg(test)]
pub mod test_support {
    use super::*;

    /// 造一张纯色测试图并写盘，返回临时文件句柄（调用方持有以控制生命周期）。
    pub fn write_test_image(w: u32, h: u32, color: Rgb<u8>) -> tempfile::NamedTempFile {
        let img = RgbImage::from_pixel(w, h, color);
        let f = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        img.save(f.path()).unwrap();
        f
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

    use super::test_support::write_test_image;

    fn decode(bytes: &[u8]) -> image::RgbImage {
        ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .to_rgb8()
    }

    fn with_background(path: &std::path::Path) -> CoverOptions {
        CoverOptions {
            background: Background::Image(path.to_path_buf()),
            ..Default::default()
        }
    }

    // 图片背景下仍产出合法 JPEG，且尺寸不变。
    #[test]
    fn renders_jpeg_with_image_background() {
        let bg = write_test_image(1146, 717, Rgb([10, 120, 200]));
        let bytes = render_cover(&["主播".to_string()], &with_background(bg.path())).unwrap();

        assert!(!bytes.is_empty());
        assert_eq!(decode_dims(&bytes), (1146, 717));
    }

    // 宽高比与画布不同的图：输出尺寸不变，且角落取自背景图而非纯黑。
    // 角落是关键取样点——它最容易在「留边」或「没铺满」时露出黑底。
    #[test]
    fn fits_wide_image_to_canvas_without_letterboxing() {
        // 极宽的图：等比缩放到覆盖高度后，左右会溢出并被裁掉
        let bg = write_test_image(3000, 500, Rgb([10, 120, 200]));
        let bytes = render_cover(&[], &with_background(bg.path())).unwrap();
        let img = decode(&bytes);

        assert_eq!(img.dimensions(), (1146, 717));
        for (x, y) in [(0, 0), (1145, 0), (0, 716), (1145, 716)] {
            let px = img.get_pixel(x, y);
            assert!(
                px[2] > px[0],
                "角落 ({x},{y}) 应取自蓝色背景图而非纯黑，实际 {px:?}"
            );
        }
    }

    // 极高的图走另一条缩放分支（按宽度覆盖），同样不该留边。
    #[test]
    fn fits_tall_image_to_canvas_without_letterboxing() {
        let bg = write_test_image(500, 3000, Rgb([10, 120, 200]));
        let bytes = render_cover(&[], &with_background(bg.path())).unwrap();
        let img = decode(&bytes);

        assert_eq!(img.dimensions(), (1146, 717));
        for (x, y) in [(0, 0), (1145, 0), (0, 716), (1145, 716)] {
            let px = img.get_pixel(x, y);
            assert!(
                px[2] > px[0],
                "角落 ({x},{y}) 应取自蓝色背景图而非纯黑，实际 {px:?}"
            );
        }
    }

    // 不存在的背景图：不报错，退化为纯黑。
    #[test]
    fn falls_back_to_black_when_background_missing() {
        let opts = with_background(std::path::Path::new("/nonexistent/background.jpg"));
        let bytes = render_cover(&["主播".to_string()], &opts).unwrap();

        assert_eq!(decode_dims(&bytes), (1146, 717));
        // 左上角在纯黑背景下应当是黑的（文字居中，不会碰到角落）
        assert_eq!(decode(&bytes).get_pixel(0, 0), &Rgb([0, 0, 0]));
    }

    // 极端宽高比：缩放到覆盖画布会需要天文数字的内存（1×3000 需约 11.8 GB）。
    // 必须提前拦下并回退纯黑，否则分配失败的 panic 会穿透 render_cover 直达投稿流程。
    #[test]
    fn falls_back_to_black_for_extreme_aspect_ratio() {
        let bg = write_test_image(1, 3000, Rgb([10, 120, 200]));
        let bytes = render_cover(&["主播".to_string()], &with_background(bg.path())).unwrap();

        assert_eq!(decode_dims(&bytes), (1146, 717));
        assert_eq!(decode(&bytes).get_pixel(0, 0), &Rgb([0, 0, 0]));
    }

    // 损坏的图片文件：同样不报错，退化为纯黑。
    #[test]
    fn falls_back_to_black_when_background_corrupt() {
        let f = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();
        std::fs::write(f.path(), b"this is not an image").unwrap();

        let bytes = render_cover(&["主播".to_string()], &with_background(f.path())).unwrap();

        assert_eq!(decode_dims(&bytes), (1146, 717));
        assert_eq!(decode(&bytes).get_pixel(0, 0), &Rgb([0, 0, 0]));
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
