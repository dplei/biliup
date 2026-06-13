use crate::server::errors::{AppError, AppResult};
use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::{Rgb, RgbImage, codecs::jpeg::JpegEncoder};
use imageproc::drawing::draw_text_mut;

/// 内嵌字体（思源黑体 CN Bold）
const FONT_BYTES: &[u8] = include_bytes!("../../../assets/fonts/SourceHanSansCN-Bold.otf");

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
    let font = FontRef::try_from_slice(FONT_BYTES)
        .map_err(|e| AppError::Custom(format!("加载内嵌字体失败: {e}")))?;

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
        .map(|l| line_width(&font, opts.base_font_px, l))
        .fold(0.0_f32, f32::max);
    if widest > max_text_w && widest > 0.0 {
        scale_px = opts.base_font_px * (max_text_w / widest);
    }
    let scale = PxScale::from(scale_px);

    // 3. 行高与垂直居中
    let scaled = font.as_scaled(scale);
    let line_h = scaled.height();
    let gap = line_h * opts.line_gap_ratio;
    let n = lines.len().max(1) as f32;
    let total_h = n * line_h + (n - 1.0).max(0.0) * gap;
    let mut y = ((opts.height as f32 - total_h) / 2.0).max(opts.margin as f32);

    // 4. 逐行行内居中绘制（带轻微描边）
    for line in &lines {
        let lw = line_width(&font, scale_px, line);
        let x = ((opts.width as f32 - lw) / 2.0).max(0.0);
        draw_line_with_stroke(&mut img, opts, &font, scale, x, y, line);
        y += line_h + gap;
    }

    // 5. 编码 JPG
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, 90)
        .encode_image(&img)
        .map_err(|e| AppError::Custom(format!("封面 JPG 编码失败: {e}")))?;
    Ok(buf)
}

/// 计算一行文字在给定字号下的像素宽度
fn line_width(font: &FontRef, px: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(px));
    let mut w = 0.0_f32;
    let mut prev = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(p) = prev {
            w += scaled.kern(p, id);
        }
        w += scaled.h_advance(id);
        prev = Some(id);
    }
    w
}

/// 画一行：先在 4 个对角偏移画描边色，再画正文，保证非黑背景上也清晰
fn draw_line_with_stroke(
    img: &mut RgbImage,
    opts: &CoverOptions,
    font: &FontRef,
    scale: PxScale,
    x: f32,
    y: f32,
    text: &str,
) {
    let off = 2_i32;
    for (dx, dy) in [(-off, -off), (off, -off), (-off, off), (off, off)] {
        draw_text_mut(
            img,
            opts.stroke_color,
            x as i32 + dx,
            y as i32 + dy,
            scale,
            font,
            text,
        );
    }
    draw_text_mut(img, opts.text_color, x as i32, y as i32, scale, font, text);
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
}
