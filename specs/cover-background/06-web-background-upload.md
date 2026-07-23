# 06 — 网页上传背景图

**What to build:** 在上传模板的编辑页面选一张图直接上传，不需要登录服务器、不需要 scp。上传后表单里就保存了这张图的文件名，投稿时的封面便以它为背景。

这一票是「摆脱终端」的核心价值所在，也是本特性里工作量最大的部分之一。

**Blocked by:** 03（需要背景字段存在才有地方保存文件名）、02（复用其路径校验函数，并需要其建立的集成测试设施）

**Status:** ready-for-agent

- [x] 新增 multipart 上传接口，接收单个图片文件
- [x] 校验扩展名与实际的图片文件头，非图片内容被拒绝
- [x] 复用 02 的路径校验函数，根目录取背景图目录——不另写一套校验规则
- [x] 文件落盘到已挂载的数据卷内，随现有备份一起被覆盖
- [x] 恶意文件名不会在目录之外产生文件
- [x] 接口返回文件名，供表单保存进背景字段
- [x] 模板编辑表单在既有的封面相关字段旁新增背景图字段与上传控件
- [x] 沿用现有表单组件库，不引入新的 UI 依赖（Semi 既有的 `Upload`/`IconUpload`）
- [x] 集成测试：合法图片上传成功且文件确实落盘
- [x] 集成测试：非图片内容被拒绝
- [x] 集成测试：恶意文件名不产生目录外的文件

## 落点

- 接口：`POST /v1/cover-backgrounds`，multipart 字段名 `file`，返回 `{"file_name": "..."}`。
- 实现：`server/api/cover_background.rs`，子路由 `cover_background_router(root)`——根目录做成参数，
  与 02 的 `static_file_router` 同一形状，集成测试才能只挂它、指定临时目录。
- 生产根目录取 `BACKGROUND_DIR`（`data/cover-backgrounds`），常量提为 `pub(crate)` 复用 03 的定义，
  不另立一个。目录由接口按需创建（03 里手动建的那步不再需要）。
- 表单：`TemplateFields.tsx` 在 `cover_template` 之后加 `cover_background` 输入框 + 上传按钮。
- 写入侧：`InsertUploadStreamer.cover_background`——03 刻意留给本票的那一项，现在前端会带上了。

## 三处有意的决定

1. **校验顺序：路径 → 单段+扩展名 → 文件头。** 第一版把单段检查写在了最前面，结果
   `resolve_within` 只剩符号链接一条分支是活的，三条路径测试全都在扩展名那关就被拒了——
   测试通过，但通过的理由和注释里写的完全是两回事。调整顺序后路径校验才真正是第一道关。
   代价是「中间目录不存在」会让 `resolve_within` 报 `RootUnavailable`，但根目录此时刚建好，
   这个错只可能来自客户端文件名，因此与越界一并归 400（有测试锁住，见下）。
2. **单段文件名检查保留，但不承担安全职责。** 它对齐的是 03 的 `background_path`——那边只认
   单段文件名，带目录的即使落了盘，投稿时也会被当成「没填」而悄悄回退纯黑。放在入口拒绝，
   是把哑失败变成明确报错，不是第二套安全规则。
3. **`StreamerConfig` 跟着加了 `cover_background`。** 不在 AC 里，但 config.toml 导入走的是
   upsert，模型里有的列都会被写一遍；不给配置项，导入就会把 `config:` 模板已配的背景清成 NULL。

## 测试为什么这么分工

九条集成测试里，只有三条是各自独占锁住某一道关的，其余锁的是行为：

- `rejects_overwriting_symlink_escaping_root` —— 独占锁 `resolve_within`，别的关卡看不出问题。
- `rejects_file_name_with_subdirectory` —— 子目录真实存在，路径校验会放行，独占锁单段检查。
- `reports_bad_request_for_missing_intermediate_directory` —— 断言的是**错误消息**而非状态码，
  因为两道关都会拒它、只是理由不同；路径校验一旦被挪到后面，这条就会失败。

穿越与绝对路径那两条，两道关都拦得下，锁的是「外面不多出文件」这个行为本身。
