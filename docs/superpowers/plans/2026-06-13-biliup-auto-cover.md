# biliup 自动封面生成（MVP）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 biliup 加「自动生成封面」能力——上传模板填了「封面文字模板」就实时生成「黑底 + 行内居中文字」JPG 作为封面上传，优先于静态 `cover_path`。

**Architecture:** 新增独立模块 `cover_generator.rs`（纯函数 `文字+选项 → JPG 字节`，可独立单测，`Background` 枚举预留背景图扩展）。在 `build_studio` 注入：`cover_template` 非空时渲染→写临时文件→赋给 `studio.cover`，复用现有 `cover_up` 上传，完后删临时文件。数据库加列 `cover_template`，模型与前端表单各加一字段。

**Tech Stack:** Rust / `image 0.25`（已依赖）+ `imageproc` + `ab_glyph` 文字绘制；内嵌思源黑体（Source Han Sans CN Bold OTF）；`tempfile 3.14`（已依赖）；sqlx 迁移；前端 Next.js + Semi Design。

参考设计：`docs/superpowers/specs/2026-06-13-biliup-auto-cover-design.md`

所有 cargo 命令在 `crates/biliup-cli` 所属 workspace 根（`/Users/leii/Code/record/biliup`）执行。

---

## File Structure

- `crates/biliup-cli/assets/fonts/SourceHanSansCN-Bold.otf` — 内嵌中文字体（新增二进制资源）
- `crates/biliup-cli/src/server/common/cover_generator.rs` — 封面渲染模块（新建，核心逻辑 + 单测）
- `crates/biliup-cli/src/server/common/mod.rs` — 注册 `cover_generator` 模块（修改）
- `crates/biliup-cli/Cargo.toml` — 加 `imageproc`、`ab_glyph` 依赖（修改）
- `crates/biliup-cli/migrations/3_add_cover_template.sql` — 加列迁移（新建）
- `crates/biliup-cli/src/server/infrastructure/models/upload_streamer.rs` — 模型加 `cover_template` 字段（修改）
- `crates/biliup-cli/src/server/common/upload.rs` — `build_studio` 注入封面生成（修改，约 268-314 行）
- `app/ui/TemplateFields.tsx` — 表单加「封面文字模板」输入框（修改，约 281-286 行）
- `app/lib/api-streamer.ts` — `StudioEntity` 加 `cover_template`（修改，约 74 行）

---

## Task 1: 加依赖与字体资源

**Files:**
- Modify: `crates/biliup-cli/Cargo.toml`
- Create: `crates/biliup-cli/assets/fonts/SourceHanSansCN-Bold.otf`

- [ ] **Step 1: 下载思源黑体 CN Bold（简中子集 OTF，约 8–9MB）**

Run:
```bash
cd /Users/leii/Code/record/biliup
mkdir -p crates/biliup-cli/assets/fonts
curl -L -o crates/biliup-cli/assets/fonts/SourceHanSansCN-Bold.otf \
  https://github.com/adobe-fonts/source-han-sans/raw/release/SubsetOTF/CN/SourceHanSansCN-Bold.otf
```
Expected: 文件存在且 > 1MB。校验：
```bash
ls -l crates/biliup-cli/assets/fonts/SourceHanSansCN-Bold.otf
file crates/biliup-cli/assets/fonts/SourceHanSansCN-Bold.otf   # 应识别为 OpenType/CFF font
```
若该 URL 失效，改用 Noto（同一字体不同发行名）：
`https://github.com/notofonts/noto-cjk/raw/main/Sans/SubsetOTF/CN/NotoSansCN-Bold.otf`（下载后仍重命名为 `SourceHanSansCN-Bold.otf`，保持代码 `include_bytes!` 路径不变）。

- [ ] **Step 2: 加 imageproc / ab_glyph 依赖**

在 `crates/biliup-cli/Cargo.toml` 的 `[dependencies]` 段（`image = "0.25"` 那一行附近）加入：
```toml
imageproc = "0.25"
ab_glyph = "0.2"
```

- [ ] **Step 3: 验证依赖可解析编译**

Run: `cargo build -p biliup-cli`
Expected: 编译成功（imageproc 0.25 与 image 0.25 版本匹配，无版本冲突报错）。

- [ ] **Step 4: 提交**

```bash
git add crates/biliup-cli/Cargo.toml crates/biliup-cli/Cargo.lock crates/biliup-cli/assets/fonts/SourceHanSansCN-Bold.otf
git commit -m "chore(cover): 加 imageproc/ab_glyph 依赖与内嵌思源黑体"
```

---

## Task 2: 实现 cover_generator 渲染模块（TDD）

**Files:**
- Create: `crates/biliup-cli/src/server/common/cover_generator.rs`
- Modify: `crates/biliup-cli/src/server/common/mod.rs`

- [ ] **Step 1: 注册模块**

在 `crates/biliup-cli/src/server/common/mod.rs` 加一行（与其它 `pub mod xxx;` 同处）：
```rust
pub mod cover_generator;
```

- [ ] **Step 2: 写最小骨架，让模块能编译**

新建 `crates/biliup-cli/src/server/common/cover_generator.rs`：
```rust
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
```

- [ ] **Step 3: 写失败测试**

在同文件末尾加：
```rust
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
```

- [ ] **Step 4: 运行测试确认失败（若编译期失败按报错修正 API）**

Run: `cargo test -p biliup-cli cover_generator -- --nocolor`
Expected: 编译通过则三个测试运行；若 `imageproc`/`ab_glyph` API 签名不符（如 `draw_text_mut` 参数顺序、`ScaleFont` 方法名 `h_advance`/`kern`/`glyph_id`/`height`、`JpegEncoder::encode_image` 入参），按编译器报错修正——核心布局逻辑不变。

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p biliup-cli cover_generator -- --nocolor`
Expected: `test result: ok. 3 passed`。

- [ ] **Step 6: 提交**

```bash
git add crates/biliup-cli/src/server/common/cover_generator.rs crates/biliup-cli/src/server/common/mod.rs
git commit -m "feat(cover): 新增 cover_generator 渲染黑底+行内居中文字封面"
```

---

## Task 3: 数据库迁移与模型字段

**Files:**
- Create: `crates/biliup-cli/migrations/3_add_cover_template.sql`
- Modify: `crates/biliup-cli/src/server/infrastructure/models/upload_streamer.rs`

- [ ] **Step 1: 写迁移**

新建 `crates/biliup-cli/migrations/3_add_cover_template.sql`：
```sql
-- 自动封面：上传模板新增「封面文字模板」字段
-- 留空=维持原有 cover_path 行为；填写=生成黑底封面，优先于 cover_path
ALTER TABLE uploadstreamers ADD COLUMN cover_template VARCHAR;
```

- [ ] **Step 2: 模型加字段**

`crates/biliup-cli/src/server/infrastructure/models/upload_streamer.rs`：
在 `UploadStreamer` 结构体内 `cover_path` 字段（约 23 行）之后加：
```rust
    /// 封面文字模板（留空=用 cover_path；填写=生成黑底封面，优先）
    pub cover_template: Option<String>,
```
在 `InsertUploadStreamer` 结构体内 `cover_path` 字段（约 70 行）之后加同样一行：
```rust
    pub cover_template: Option<String>,
```

- [ ] **Step 3: 编译验证（确保 ormlite 派生与新列一致）**

Run: `cargo build -p biliup-cli`
Expected: 编译成功。ormlite 按字段名映射到表列 `cover_template`，与迁移列名一致。

- [ ] **Step 4: 提交**

```bash
git add crates/biliup-cli/migrations/3_add_cover_template.sql crates/biliup-cli/src/server/infrastructure/models/upload_streamer.rs
git commit -m "feat(cover): 加 cover_template 字段与迁移"
```

---

## Task 4: 接入 build_studio（生成临时封面并上传）

**Files:**
- Modify: `crates/biliup-cli/src/server/common/upload.rs`（`build_studio`，约 268-314 行）

- [ ] **Step 1: 引入模块依赖**

在 `upload.rs` 顶部 `use` 区加：
```rust
use crate::server::common::cover_generator::{CoverOptions, render_cover};
```

- [ ] **Step 2: 在 build_studio 内、`.build();` 之后、封面上传 `if` 之前插入生成逻辑**

在 `build_studio` 中，紧接 `.build();`（约 304 行）之后、`// 处理封面上传`（约 305 行）之前，插入：
```rust
    // 自动封面：cover_template 非空则生成黑底封面，覆盖 studio.cover
    let mut _auto_cover_tmp: Option<tempfile::NamedTempFile> = None;
    if let Some(tpl) = upload_config
        .cover_template
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let text = recorder.format(tpl);
        let lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        match render_cover(&lines, &CoverOptions::default()) {
            Ok(bytes) => match tempfile::Builder::new()
                .prefix("biliup-cover-")
                .suffix(".jpg")
                .tempfile()
            {
                Ok(mut f) => {
                    use std::io::Write;
                    if let Err(e) = f.write_all(&bytes).and_then(|_| f.flush()) {
                        error!(e=?e, "写入临时封面失败，回退到 cover_path");
                    } else {
                        studio.cover = f.path().to_string_lossy().into_owned();
                        _auto_cover_tmp = Some(f); // 持有到上传完毕再销毁
                    }
                }
                Err(e) => error!(e=?e, "创建临时封面文件失败，回退到 cover_path"),
            },
            Err(e) => error!(e=?e, "生成自动封面失败，回退到 cover_path"),
        }
    }
```
说明：`_auto_cover_tmp` 持有 `NamedTempFile`，函数返回时自动删除临时文件；上传 `cover_up` 在其后、同一函数作用域内完成，故文件在上传期间有效。

- [ ] **Step 3: 编译验证**

Run: `cargo build -p biliup-cli`
Expected: 编译成功（`error!` 宏、`tempfile` 均已在 crate 内可用）。

- [ ] **Step 4: 写集成测试——cover_template 非空时 studio.cover 指向有效 JPG**

由于 `build_studio` 依赖 `BiliBili`（需网络上传），不直接测整函数。改为测「生成+落盘」这段可抽出的纯逻辑：在 `cover_generator.rs` 增加一个便捷函数并测它。

在 `cover_generator.rs` 加：
```rust
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
```
并把 Step 2 中 build_studio 的生成+落盘替换为调用它，简化分支：
```rust
        match render_to_tempfile(&lines, &CoverOptions::default()) {
            Ok(f) => {
                studio.cover = f.path().to_string_lossy().into_owned();
                _auto_cover_tmp = Some(f);
            }
            Err(e) => error!(e=?e, "生成自动封面失败，回退到 cover_path"),
        }
```
在 `cover_generator.rs` 测试模块加：
```rust
    #[test]
    fn render_to_tempfile_writes_valid_jpeg() {
        let f = render_to_tempfile(&["主播".to_string()], &CoverOptions::default()).unwrap();
        let bytes = std::fs::read(f.path()).unwrap();
        assert_eq!(decode_dims(&bytes), (1146, 717));
    }
```

- [ ] **Step 5: 运行测试**

Run: `cargo test -p biliup-cli cover_generator -- --nocolor`
Expected: 4 passed。

- [ ] **Step 6: 整体编译 + clippy**

Run: `cargo build -p biliup-cli && cargo clippy -p biliup-cli -- -D warnings`
Expected: 无错误。`_auto_cover_tmp` 前缀下划线避免 unused 警告。

- [ ] **Step 7: 提交**

```bash
git add crates/biliup-cli/src/server/common/upload.rs crates/biliup-cli/src/server/common/cover_generator.rs
git commit -m "feat(cover): build_studio 接入自动封面（cover_template 优先于 cover_path）"
```

---

## Task 5: 前端表单与类型

**Files:**
- Modify: `app/ui/TemplateFields.tsx`（约 281-286 行，`field="cover_path"` 之后）
- Modify: `app/lib/api-streamer.ts`（`StudioEntity`，约 74 行）

- [ ] **Step 1: 类型加字段**

`app/lib/api-streamer.ts` 的 `StudioEntity` 接口里 `cover_path: string;`（约 74 行）之后加：
```ts
	cover_template?: string;
```

- [ ] **Step 2: 表单加输入框**

`app/ui/TemplateFields.tsx` 中 `field="cover_path"` 的 `<Input ... />`（约 281-286 行）之后加：
```tsx
        <Input
          field="cover_template"
          label="封面文字模板"
          style={{ width: 464 }}
          placeholder="留空用上方封面图；示例：{streamer}\n%Y-%m-%d %H点场"
          extraText="填写后自动生成黑底封面，优先于「视频封面」。支持 {streamer}/{title}/{url} 与时间占位符，\n 换行"
        />
```

- [ ] **Step 3: 前端类型检查/构建**

Run: `npm run build`（或项目既有的 lint/typecheck 脚本，如 `npm run lint`）
Expected: 通过，无类型错误。
（若本机未装前端依赖且本任务仅验证字段，最低限度执行 `npx tsc --noEmit` 校验类型。）

- [ ] **Step 4: 提交**

```bash
git add app/ui/TemplateFields.tsx app/lib/api-streamer.ts
git commit -m "feat(cover): 上传模板表单加「封面文字模板」字段"
```

---

## Task 6: 手动端到端验证（非自动化）

**Files:** 无（运行验证）

- [ ] **Step 1: 起服务，建/改一个上传模板，填封面文字模板**

启动 biliup 服务（按项目既有方式，如 `cargo run -p biliup-cli -- ...` 或 Docker），进 Web「上传管理」编辑某模板，「封面文字模板」填：`{streamer}\n%Y-%m-%d %H点场`，保存。

- [ ] **Step 2: 触发一次投稿（或用一段已录好的短视频走上传流程），观察日志与结果**

Expected:
- 日志无「生成自动封面失败/回退」错误。
- B站稿件封面为黑底白字两行：主播名 + 日期时间。
- 临时文件（`biliup-cover-*.jpg`）上传后已被清理（系统临时目录无残留）。

- [ ] **Step 3: 回归——把封面文字模板清空保存，再投一次**

Expected: 退回原行为，使用「视频封面」`cover_path`（或无封面），不生成黑底封面。

---

## 备注：分支与合并

- 本特性分支 `feat/auto-cover` 基于 `master`，可直接对（自己的）`master` 提 PR。
- 要在生产 `dev` 用：`git checkout dev && git merge feat/auto-cover`（dev 含 master 全部内容，合并干净）。
- 上游 PR（fork 的 upstream）：单独挑选 `crates/`、`app/`、`migrations/` 的代码改动，不包含 `docs/superpowers/` 内部文档。
