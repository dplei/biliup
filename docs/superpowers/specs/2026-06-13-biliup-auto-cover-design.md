# biliup 自动封面生成（MVP）设计

日期：2026-06-13
状态：已确认设计，待写实现计划

## 背景与目标

参考 B 站录播号「格温的录像机」的封面机制：每条投稿封面是**预制底图（模糊）+ 程序化叠加文字（主播名 + 直播时间）**。我们要给 biliup 加一个「自动生成封面」能力，一站式集成到现有投稿链路。

本版（MVP）只做 **黑底 + 文字**，架构上为「可配置背景图」预留扩展口，下一版再加。

## 功能行为

每个**上传模板**（`UploadStreamer`）新增字段 `cover_template`（UI 名「封面文字模板」）：

- **留空** → 维持现状：用静态配置的 `cover_path`，没配则无封面。
- **填写** → 投稿前实时生成一张 **1146×717（B 站推荐 16:10）黑底 JPG**，渲染模板文字，作为封面上传，**优先级高于** `cover_path`。

文字模板复用现有 `Recorder::format()`：

- 支持占位符 `{streamer}`、`{title}`、`{url}` 与 strftime（如 `%Y-%m-%d`、`%H`）。
- **换行用 `\n` 分隔**，逐行渲染。
- **每一行各自水平居中（行内居中）**，多行作为整体垂直居中。
- 默认示例模板：`{streamer}\n%Y-%m-%d %H点场` → 渲染成两行，与参考效果一致。

## 模块划分（隔离 + 可扩展）

新增独立模块 `crates/biliup-cli/src/server/common/cover_generator.rs`，单一职责：**「文字 + 选项 → JPG 字节」**，不依赖上传逻辑，可独立单测。

```rust
pub struct CoverOptions {
    pub width: u32,            // 默认 1146
    pub height: u32,          // 默认 717
    pub background: Background,
    // 文字颜色/字号等留默认即可，后续可扩展
}

pub enum Background {
    Black,
    // 下一版：Image(PathBuf) —— 加载、缩放、高斯模糊
}

/// 输入已渲染好的多行文字，输出 JPG 字节
pub fn render_cover(lines: &[String], opts: &CoverOptions) -> AppResult<Vec<u8>>;
```

- 本版只实现 `Background::Black` 分支。
- 下一版加背景图时**只动这个模块**，调用方与数据流不变。

### 文字渲染细节

- 依赖：`image`（已在 `biliup-cli/Cargo.toml`）补充 `imageproc` + `ab_glyph`。
- 字体：**内嵌思源黑体（Source Han Sans，开源可商用）**，通过 `include_bytes!` 打进二进制。
  - 取舍：全量 CJK 字体十几 MB，会显著增大二进制。优先选用**子集化 / 单字重**的思源黑体，控制在可接受体积（目标 1–3 MB 量级，覆盖常用汉字 + 数字 + 标点 + 基本拉丁）。具体字体文件在实现阶段确定并放入仓库（如 `crates/biliup-cli/assets/fonts/`）。
- 颜色：白色文字。为兼容未来非黑背景，渲染时带轻微深色描边（`stroke`），黑底上无副作用。
- 字号：按画布宽度与最长行自适应，保证不溢出边距。
- 排版：每行各自水平居中（行内居中），多行整体垂直居中。

## 集成点（改动最小）

只改 `build_studio`（`crates/biliup-cli/src/server/common/upload.rs:336`）：

1. 读取 `upload_config.cover_template`。
2. 非空时：
   - `let text = recorder.format(cover_template)`（占位符 + strftime 已被替换）。
   - 按 `\n` 切成 `lines`。
   - `let bytes = render_cover(&lines, &CoverOptions::default())`。
   - 写入临时文件（系统临时目录），把路径赋给 `studio.cover`（覆盖 `cover_path`）。
3. 空时：维持现有 `cover_path` 逻辑，零改动。
4. 后续 `cover_up` 原样上传。上传完成后删除临时文件。

其余投稿逻辑不变。

## 数据库与配置

- 迁移：`upload_streamer` 表新增列 `cover_template TEXT`（可空）。
- 模型：`UploadStreamer`、`InsertUploadStreamer` 各加 `pub cover_template: Option<String>`。
- Web 表单：上传模板编辑页新增「封面文字模板」输入框，带占位符说明与默认示例。

## 测试

- `render_cover`：给定多行文字与默认选项，产出非空、尺寸为 1146×717 的合法 JPG（可解码校验尺寸）。
- 模板渲染：`Recorder::format()` 对 `{streamer}`、strftime、`\n` 的处理符合预期（占位符替换正确、多行切分正确）。
- 集成：`build_studio` 在 `cover_template` 非空/为空两种路径下，分别走「生成封面」与「沿用 cover_path」。

## 非目标（YAGNI / 下一版）

- 可配置背景图（绝活英雄图 / 分区统一图）+ 高斯模糊 + 黑底兜底链。
- 抓视频帧作背景。
- 字体/字号/颜色/排版的细粒度 UI 配置。
- 每主播专属底图映射的数据结构。

这些都通过 `Background` 枚举与 `CoverOptions` 预留，后续增量实现，不返工。
