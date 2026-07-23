//! 本地封面预览：挑背景图、试压暗与模糊参数时用它出样图。
//!
//! 关键设计：压暗与模糊**只存在于这里**，不进数据库、不进服务端渲染路径。
//! 用法是先在本地把满意的效果烘焙进一张成品图，再把那张图上传给服务端——
//! 服务端因此永远只需要认识「一张图」，不必知道任何图像处理参数。
//!
//! 渲染本身复用 `render_cover`，所以本地看到的效果就是实际投稿的效果。

use crate::server::common::cover_generator::{
    Background, CoverOptions, load_fitted, render_cover, split_template_lines,
};
use crate::server::errors::{AppError, AppResult};
use error_stack::ResultExt;
use image::RgbImage;
use std::path::{Path, PathBuf};

/// 渲染一张预览封面并写入 `output`。
///
/// `background` 为 None 时用纯黑底；`dim` 为压暗百分比（0-100），`blur` 为高斯模糊半径（0 为不模糊）。
/// `background_only` 为真时只输出处理好的背景图、不画文字——那张图就是可以直接上传的成品背景。
pub fn cover_preview(
    text: &str,
    background: Option<PathBuf>,
    output: &Path,
    dim: u8,
    blur: f32,
    background_only: bool,
) -> AppResult<()> {
    let opts = CoverOptions::default();

    let baked = match background {
        Some(path) => Some(bake_background(&path, &opts, dim, blur)?),
        None => None,
    };

    // 只要背景：直接存盘，不进渲染器。这是「把调好的效果烘焙成成品图」的产出口。
    if background_only {
        let Some(img) = baked else {
            return Err(
                AppError::Custom("--background-only 需要同时指定 --background".into()).into(),
            );
        };
        return img.save(output).change_context_lazy(|| {
            AppError::Custom(format!("写入背景图失败: {}", output.display()))
        });
    }

    let opts = CoverOptions {
        // 交内存位图而非临时文件：省掉一次落盘 + 一次重新解码，
        // 也就省掉一次有损压缩——本地看到的才真等于线上产出。
        background: match baked {
            Some(img) => Background::Bitmap(img),
            None => Background::Black,
        },
        ..opts
    };

    let lines = split_template_lines(text);
    let bytes = render_cover(&lines, &opts)?;

    std::fs::write(output, &bytes).change_context_lazy(|| {
        AppError::Custom(format!("写入预览封面失败: {}", output.display()))
    })?;

    Ok(())
}

/// 适配画布 → 模糊 → 压暗，返回可直接当背景用的位图。
///
/// 先适配再处理：模糊的代价随像素数增长，先缩到画布尺寸能快一个数量级。
/// 顺序上先模糊再压暗——反过来会让模糊把已压暗的边缘与未压暗区域混在一起，
/// 压暗程度就不再均匀。
fn bake_background(path: &Path, opts: &CoverOptions, dim: u8, blur: f32) -> AppResult<RgbImage> {
    let mut img = load_fitted(path, opts.width, opts.height)
        .change_context_lazy(|| AppError::Custom(format!("读取背景图失败: {}", path.display())))?;

    // 负值与 NaN 在此一并被排除（NaN 的任何比较都为假）
    if blur > 0.0 {
        img = image::imageops::blur(&img, blur);
    }
    if dim > 0 {
        darken(&mut img, dim);
    }

    Ok(img)
}

/// 按百分比压暗：`percent` 为 100 时全黑。等价于在图上盖一层该不透明度的黑色遮罩。
fn darken(img: &mut RgbImage, percent: u8) {
    let keep = (100 - percent.min(100)) as u32;
    for px in img.pixels_mut() {
        for c in px.0.iter_mut() {
            *c = ((*c as u32 * keep) / 100) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::common::cover_generator::test_support::write_test_image;
    use image::{ImageReader, Rgb};

    fn decode(path: &Path) -> image::RgbImage {
        ImageReader::open(path)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .to_rgb8()
    }

    #[test]
    fn writes_jpeg_with_canvas_dimensions() {
        let out = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();
        cover_preview("测试主播\\n2026-07-23", None, out.path(), 0, 0.0, false).unwrap();

        assert_eq!(decode(out.path()).dimensions(), (1146, 717));
    }

    #[test]
    fn renders_with_image_background() {
        let bg = write_test_image(2000, 1000, Rgb([10, 120, 200]));
        let out = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();

        cover_preview(
            "主播",
            Some(bg.path().to_path_buf()),
            out.path(),
            0,
            0.0,
            false,
        )
        .unwrap();

        let img = decode(out.path());
        assert_eq!(img.dimensions(), (1146, 717));
        let px = img.get_pixel(0, 0);
        assert!(px[2] > px[0], "角落应取自蓝色背景图，实际 {px:?}");
    }

    // 压暗后角落应明显变暗，但不至于全黑。
    #[test]
    fn dim_darkens_background() {
        let bg = write_test_image(1146, 717, Rgb([200, 200, 200]));
        let out = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();

        cover_preview(
            "主播",
            Some(bg.path().to_path_buf()),
            out.path(),
            60,
            0.0,
            false,
        )
        .unwrap();

        let px = *decode(out.path()).get_pixel(0, 0);
        // 200 * 40% = 80，留出 JPEG 有损压缩的余量
        assert!(
            (70..=90).contains(&px[0]),
            "压暗 60% 后应在 80 附近，实际 {px:?}"
        );
    }

    #[test]
    fn dim_100_yields_black_background() {
        let bg = write_test_image(1146, 717, Rgb([200, 200, 200]));
        let out = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();

        cover_preview(
            "主播",
            Some(bg.path().to_path_buf()),
            out.path(),
            100,
            0.0,
            false,
        )
        .unwrap();

        let px = *decode(out.path()).get_pixel(0, 0);
        assert!(px[0] < 10, "压暗 100% 应近乎全黑，实际 {px:?}");
    }

    // --background-only：产出的是纯背景图，不带文字。这是票 04 的 skill 拿来上传的成品。
    #[test]
    fn background_only_writes_bare_background() {
        let bg = write_test_image(2000, 1000, Rgb([200, 200, 200]));
        let out = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();

        cover_preview(
            "不该出现的文字",
            Some(bg.path().to_path_buf()),
            out.path(),
            50,
            0.0,
            true,
        )
        .unwrap();

        let img = decode(out.path());
        assert_eq!(img.dimensions(), (1146, 717), "成品背景须是画布尺寸");

        // 画面正中央若有文字必然出现接近纯白的像素；纯背景则处处是压暗后的灰。
        let center = *img.get_pixel(573, 358);
        assert!(
            center[0] < 140,
            "不该画上文字，中心应为压暗后的灰，实际 {center:?}"
        );
    }

    #[test]
    fn background_only_without_background_is_rejected() {
        let out = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();
        assert!(cover_preview("主播", None, out.path(), 0, 0.0, true).is_err());
    }

    // 模糊会把锐利边界抹开：取边界附近的像素，应落在两色之间而非仍是原色。
    #[test]
    fn blur_softens_hard_edges() {
        let mut src = RgbImage::from_pixel(1146, 717, Rgb([0, 0, 0]));
        for y in 0..717 {
            for x in 573..1146 {
                src.put_pixel(x, y, Rgb([255, 255, 255]));
            }
        }
        let bg = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
        src.save(bg.path()).unwrap();

        let out = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();
        cover_preview(
            "",
            Some(bg.path().to_path_buf()),
            out.path(),
            0,
            20.0,
            false,
        )
        .unwrap();

        let px = *decode(out.path()).get_pixel(573, 10);
        assert!(
            (30..=225).contains(&px[0]),
            "边界处应被模糊成中间色，实际 {px:?}"
        );
    }

    // 背景图有问题时渲染器会退回纯黑，但预览子命令应当据实报错——
    // 本地调参时悄悄给一张黑图，比直接报错更难排查。
    #[test]
    fn reports_error_for_unreadable_background() {
        let out = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();
        let missing = PathBuf::from("/nonexistent/bg.jpg");

        assert!(cover_preview("主播", Some(missing), out.path(), 0, 0.0, false).is_err());
    }
}
