# 03 — 上传模板级背景生效

**What to build:** 在上传模板上配置一个背景图文件名之后，这个模板产出的封面就以那张图为背景。不配置的模板维持现在的纯黑底，升级前后行为完全一致。

本票打通「数据库配置 → 解析 → 渲染 → 实际投稿」这条链路。此阶段还没有网页表单，配置值可以直接写进数据库来验证。

**Blocked by:** 01（需要图片背景的渲染能力先存在）

**Status:** ready-for-agent

- [x] 上传模板表新增一个可空的背景字段，迁移文件编号顺延，写法沿用既有迁移：单条 ALTER TABLE，不修改既有列、不做数据回填
- [x] 字段存文件名而非绝对路径，实际路径在运行时拼接（更换挂载点或迁移部署时数据无需修改）
- [x] 新增背景解析纯函数，不触碰数据库、不做 IO
- [x] 空值语义沿用既有约定：NULL 与空白字符串都视作「未填写」
- [x] 解析函数放在构建投稿信息的同一模块内，与既有的来源解析函数并列摆放
- [x] 构建投稿信息时用解析结果替换写死的纯黑背景，其余逻辑（文字模板展开、按换行标记切行、临时文件生命周期、优先于既有封面路径字段）全部不变
- [x] 升级后未配置背景的既有模板，产出的封面与升级前逐像素一致
- [x] 解析函数单测：未配置 → 纯黑
- [x] 解析函数单测：已配置 → 图片背景
- [x] 解析函数单测：空白字符串与 NULL 等价，均回退到纯黑

## 落点

- 迁移：`crates/biliup-cli/migrations/7_add_cover_background.sql`，单条 `ALTER TABLE uploadstreamers ADD COLUMN cover_background VARCHAR`。
- 字段：`UploadStreamer.cover_background`（读取侧模型）。
- 解析：`upload.rs` 的 `resolve_background`，紧邻 `resolve_source`；目录常量 `BACKGROUND_DIR = "data/cover-backgrounds"`。
- 接线：`build_studio` 里把写死的 `CoverOptions::default()` 换成带解析结果的选项，其余不动。

**背景目录取 `data/cover-backgrounds/`**：与数据库文件（`data/data.sqlite3`）同在 `data/` 下，因此天然落在现有备份范围内；和数据库路径一样是相对工作目录的，容器里工作目录即挂载卷，换挂载点无需改数据。

## 三处有意的决定

1. **字段只加在 `UploadStreamer`，没加进 `InsertUploadStreamer`。** 网页编辑模板走的是
   `add_upload_streamer_endpoint` → `update_all_fields`，提交的是前端拼的完整 JSON。前端在
   票 06 之前不知道这个字段，缺项会反序列化成 `None`，`update_all_fields` 就会把手工配好的
   背景清成 NULL。写入侧留给票 06 与表单一起加，那时前端才会带上这一项。
   （review 已核实 ormlite 的 `update_all_fields` 只 SET 本模型声明的列，config.toml
   导入路径同理，所以不加反而是保住手工值的那一侧。）
   副作用是本阶段只能直接写库配置——票里本来就是这么说的。
2. **解析函数只认「一个文件名」，其余一律当没填。** 带目录、`..`、绝对路径都回退纯黑。
   起因是 review 发现 `Path::join` 碰上绝对路径会把基路径整段丢掉（`join("/etc/x")`
   就是 `/etc/x`），库里一个值就能把渲染器指到背景图目录之外。原先的理由「校验落点在
   票 06 的上传接口」只覆盖上传的文件，覆盖不了表单那个自由文本字段直接写进 DB 的值。
   拦截靠 `Path::components` 判断，仍是纯函数、不做 IO，没有引入
   `path_safety::resolve_within`（那个要 `canonicalize`，会破坏 AC 的纯函数要求）。
3. **额外加了两个数据库往返测试。** AC 没要求，但迁移是本票唯一「跑起来才知道对不对」的
   部分：列名或类型对不上，线上一读就炸。跟着 `missing_segment.rs` 的 `test_pool` 先例，
   用临时库跑完整迁移再读回来。第二条同时锁住升级路径——既有模板不会因为多一列而读不出。

## 「逐像素一致」这条 AC 怎么保证的

原本写了一条测试，渲两张图比字节。review 两轴都指出它是同义反复：两边都是当前代码的
`CoverOptions::default()`，没有升级前的基准字节，也没走 `build_studio`——接错线它照样通过，
却要花 11.5s。已删除。

实际保证来自三处：`resolve_background(None)` 解析为纯黑（有单测）；`build_studio` 里除
`background` 外全部 `..CoverOptions::default()`（改动可见）；纯黑渲染本身由
`cover_generator` 既有的十个测试锁住。

## 本阶段怎么验证

还没有表单，直接写库：

```bash
mkdir -p data/cover-backgrounds   # 目录由票 06 的上传接口负责创建，现在得手动建
cp 你的背景图.jpg data/cover-backgrounds/aurora.jpg
sqlite3 data/data.sqlite3 "UPDATE uploadstreamers SET cover_background='aurora.jpg' WHERE template_name='你的模板名';"
```

下一次投稿的自动封面就会压在这张图上。图不存在或损坏时回退纯黑并打日志（`读取封面背景图失败`），投稿不受影响。
