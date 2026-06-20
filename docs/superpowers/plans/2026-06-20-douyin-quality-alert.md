# 抖音画质降级告警 + 录制画质 tag 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 抖音实际录到的画质低于可配阈值（默认蓝光 uhd）时通过 webhook 推送提醒，并在直播管理页录制中的房间旁显示当前画质 tag。

**Architecture:** 抖音选流 `select_stream_url` 把实际选中的画质代码冒泡到 `LiveStream.recording_quality`；录制启动处（`start_download_workflow`）读合并后的配置判定是否低于阈值并复用 `notify_alert(cookie_health_webhook,…)` 推送，同时把画质写入 `Worker.recording_quality`（断流复查刷新、下播清空）；`get_streamers_endpoint` 把它返回给前端渲染 tag。

**Tech Stack:** Rust（axum / tokio / serde_json）后端，Next.js + `@douyinfe/semi-ui` 前端。

## Global Constraints

- 画质排名（越小越高）：`["origin","uhd","hd","sd","ld","md"]`，未知画质视为最低档。「蓝光」= `uhd`。
- 画质代码→中文：origin=原画, uhd=蓝光, hd=超清, sd=高清, ld=标清, md=流畅。
- 告警阈值缺省（None/空字符串）按 `"uhd"` 处理；值为 `"off"` 时关闭告警。
- 告警仅抖音；复用现有 `cookie_health_webhook`，不新增 webhook 配置。
- 每场开播只推一次：告警只在 `start_download_workflow` 触发；断流复查只刷新 tag、不推送。
- 新字段命名固定：`recording_quality`（LiveStream / Worker / 接口响应）、`douyin_quality_alert`（Config / 表单 field）。
- 工作目录均为仓库根 `/Users/leii/Code/record/biliup`；提交在 `dev` 分支。

---

### Task 1: LiveStream 携带实际画质 + 抖音填充

**Files:**
- Modify: `crates/biliup/src/downloader/live/mod.rs:303-316`（`LiveStream` 加字段）
- Modify: `crates/biliup/src/downloader/live/douyin.rs:327-402`（`select_stream_url` 返回选中画质）、`douyin.rs:98` 与 `:113-126`（写入字段）
- Modify（各加一行 `recording_quality: None,` 到 `LiveStream { … }` 字面量）：
  `acfun.rs:76`、`inke.rs:85`、`missevan.rs:89`、`yy.rs:146`、`kilakila.rs:93`、`ttinglive.rs:77`、`afreecatv.rs:80`、`youtube.rs:119`、`twitcasting.rs:91`、`cc.rs:84`、`picarto.rs:92`、`douyu.rs:90`、`bigo.rs:88`、`general.rs:94`、`niconico.rs:76`、`twitch.rs:181`、`twitch.rs:294`、`bilibili.rs:119`、`huya.rs:99`、`kuaishou.rs:89`（均在 `crates/biliup/src/downloader/live/`）
- Test: `crates/biliup/src/downloader/live/douyin.rs`（文件内 `#[cfg(test)]`）

**Interfaces:**
- Produces: `LiveStream.recording_quality: Option<String>`（抖音为 `Some(画质代码)`，其它平台 `None`）。
- Produces: `fn select_quality_code(available: &[&str], requested: &str) -> Option<&'static str>`（pure，供选流与单测）。

- [ ] **Step 1: 写失败测试（纯函数选档逻辑）**

在 `crates/biliup/src/downloader/live/douyin.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod quality_tests {
    use super::select_quality_code;

    #[test]
    fn requested_available_returns_itself() {
        assert_eq!(select_quality_code(&["origin", "hd"], "origin"), Some("origin"));
    }

    #[test]
    fn missing_requested_falls_back_to_next_lower() {
        // 请求 origin 不在，向低优先：uhd
        assert_eq!(select_quality_code(&["uhd", "hd"], "origin"), Some("uhd"));
    }

    #[test]
    fn falls_back_upward_when_only_higher_exists() {
        // 请求 sd 不在、低档也没有，回退到更高档 hd
        assert_eq!(select_quality_code(&["origin", "hd"], "sd"), Some("hd"));
    }

    #[test]
    fn none_when_empty() {
        assert_eq!(select_quality_code(&[], "origin"), None);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p biliup quality_tests 2>&1 | tail -20`
Expected: 编译失败，`cannot find function select_quality_code`。

- [ ] **Step 3: 提取纯函数并改造 `select_stream_url` 返回选中画质**

在 `douyin.rs` 顶层（`impl DouyinLive` 之外）新增纯函数，并复用进 `select_stream_url`：

```rust
/// 在 available 列表中，按 origin>uhd>hd>sd>ld>md 的优先级为 requested 选一档可用画质：
/// 命中则用之；否则先往更低档找，再往更高档找；都没有返回 None。
fn select_quality_code(available: &[&str], requested: &str) -> Option<&'static str> {
    const ITEMS: [&str; 6] = ["origin", "uhd", "hd", "sd", "ld", "md"];
    let requested = if ITEMS.contains(&requested) { requested } else { "origin" };
    let idx = ITEMS.iter().position(|i| *i == requested).unwrap_or(0);
    let has = |q: &str| available.contains(&q);
    if has(requested) {
        return ITEMS.iter().copied().find(|i| *i == requested);
    }
    ITEMS[idx + 1..]
        .iter()
        .copied()
        .find(|i| has(i))
        .or_else(|| ITEMS[..idx].iter().rev().copied().find(|i| has(i)))
}
```

把 `select_stream_url` 的签名从返回 `LiveResult<String>` 改为 `LiveResult<(String, String)>`（url, 画质代码）。改 `douyin.rs:348-402` 区间：

- `true_origin` 提前返回分支改为：
  ```rust
          return Ok((
              url.replace("&only_audio=1", "").replace("http://", "https://"),
              "origin".to_string(),
          ));
  ```
- 下半段把现有 `quality_items`/`selected_quality` 选择逻辑替换为：
  ```rust
          let available: Vec<&str> = stream_data.keys().map(String::as_str).collect();
          let selected_quality = select_quality_code(&available, &self.douyin_quality)
              .ok_or_else(|| LiveError::custom("抖音没有可用清晰度"))?;

          let protocol = if self.douyin_protocol == "hls" { "hls" } else { "flv" };

          let url = stream_data
              .get(selected_quality)
              .and_then(|quality| quality.pointer(&format!("/main/{protocol}")))
              .and_then(Value::as_str)
              .filter(|url| !url.is_empty())
              .map(|url| url.replace("http://", "https://"))
              .ok_or_else(|| LiveError::custom("抖音可用直播流为空"))?;
          Ok((url, selected_quality.to_string()))
  ```

在 `check_stream`（`douyin.rs:98`）改为接收二元组并写入字段：

```rust
        let (raw_stream_url, recording_quality) = self.select_stream_url(&room_info)?;
```
并在 `LiveStream { … }`（`douyin.rs:113-126`）追加：
```rust
                recording_quality: Some(recording_quality),
```

- [ ] **Step 4: 给 LiveStream 加字段并补齐所有构造点**

在 `mod.rs:303-316` 的 `LiveStream` 末尾字段后加：
```rust
    /// 实际选中的画质代码（origin/uhd/...）。仅抖音填充，其它平台为 None。
    #[serde(default)]
    pub recording_quality: Option<String>,
```
然后在 **File 列表中除 douyin.rs 外的 20 个构造点**，每个 `LiveStream { … }` 字面量内补一行：
```rust
            recording_quality: None,
```

- [ ] **Step 5: 运行测试与编译**

Run: `cargo test -p biliup quality_tests 2>&1 | tail -20 && cargo build -p biliup 2>&1 | tail -5`
Expected: 4 个测试 PASS；`biliup` crate 编译通过（无 missing field 错误）。

- [ ] **Step 6: Commit**

```bash
git add crates/biliup/src/downloader/live
git commit -m "feat(douyin): 选流返回实际画质并经 LiveStream.recording_quality 暴露"
```

---

### Task 2: 告警阈值配置 + 判定/展示纯函数

**Files:**
- Modify: `crates/biliup-cli/src/server/config.rs:157-172`（加 `douyin_quality_alert` 字段）
- Modify: `crates/biliup-cli/src/server/common/cookie_health.rs`（加 `quality_below_alert` + `quality_display` + 单测）

**Interfaces:**
- Produces: `Config.douyin_quality_alert: Option<String>`（自动进入 `ConfigPatch`，全局+单房间覆盖白送）。
- Produces: `pub fn quality_below_alert(actual: &str, threshold: Option<&str>) -> bool`
- Produces: `pub fn quality_display(code: &str) -> &'static str`

- [ ] **Step 1: 写失败测试**

在 `cookie_health.rs` 末尾追加：

```rust
#[cfg(test)]
mod quality_alert_tests {
    use super::{quality_below_alert, quality_display};

    #[test]
    fn below_threshold_triggers() {
        assert!(quality_below_alert("hd", Some("uhd")));
    }

    #[test]
    fn at_or_above_threshold_no_trigger() {
        assert!(!quality_below_alert("uhd", Some("uhd")));
        assert!(!quality_below_alert("origin", Some("uhd")));
    }

    #[test]
    fn off_disables() {
        assert!(!quality_below_alert("md", Some("off")));
    }

    #[test]
    fn none_or_empty_defaults_to_uhd() {
        assert!(quality_below_alert("hd", None));
        assert!(quality_below_alert("hd", Some("")));
        assert!(!quality_below_alert("uhd", None));
    }

    #[test]
    fn display_maps_codes() {
        assert_eq!(quality_display("uhd"), "蓝光");
        assert_eq!(quality_display("hd"), "超清");
        assert_eq!(quality_display("xxx"), "xxx");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p biliup-cli quality_alert_tests 2>&1 | tail -20`
Expected: 编译失败，找不到 `quality_below_alert` / `quality_display`。

- [ ] **Step 3: 实现两个纯函数**

在 `cookie_health.rs`（`notify_alert` 附近）加。`quality_display` 对未知码需原样返回，故返回 `String`：

```rust
/// 画质代码排名，越小越高；未知画质排到最低。
fn quality_rank(code: &str) -> usize {
    const ITEMS: [&str; 6] = ["origin", "uhd", "hd", "sd", "ld", "md"];
    ITEMS.iter().position(|i| *i == code).unwrap_or(ITEMS.len())
}

/// 画质代码 → 中文展示名（未知码原样返回）。
pub fn quality_display(code: &str) -> String {
    match code {
        "origin" => "原画".to_string(),
        "uhd" => "蓝光".to_string(),
        "hd" => "超清".to_string(),
        "sd" => "高清".to_string(),
        "ld" => "标清".to_string(),
        "md" => "流畅".to_string(),
        other => other.to_string(),
    }
}

/// 实际画质是否低于告警阈值（应推送）。
/// threshold 为 None/空 → 默认 "uhd"；"off" → 关闭（恒 false）。
pub fn quality_below_alert(actual: &str, threshold: Option<&str>) -> bool {
    let threshold = threshold.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("uhd");
    if threshold == "off" {
        return false;
    }
    quality_rank(actual) > quality_rank(threshold)
}
```

并把测试里 `assert_eq!(quality_display("uhd"), "蓝光")` 等改为与 `String` 比较（`"蓝光"` 字面量可直接和 `String` 用 `assert_eq!` 比较，需写成 `assert_eq!(quality_display("uhd"), "蓝光")` —— Rust 中 `String == &str` 成立，保持原样即可）。

- [ ] **Step 4: 加 Config 字段**

在 `config.rs` 抖音设置区（`:172` 的 `douyin_true_origin` 之后）加：

```rust
    /// 抖音画质降级告警阈值：实际录到的画质低于此档时 webhook 推送。
    /// 取值同画质（origin/uhd/hd/sd/ld/md），"off"=关闭；缺省视为 "uhd"（蓝光）。
    #[serde(default)]
    pub douyin_quality_alert: Option<String>,
```

- [ ] **Step 5: 运行测试与编译**

Run: `cargo test -p biliup-cli quality_alert_tests 2>&1 | tail -20`
Expected: 5 个测试 PASS。

- [ ] **Step 6: Commit**

```bash
git add crates/biliup-cli/src/server/config.rs crates/biliup-cli/src/server/common/cookie_health.rs
git commit -m "feat: 抖音画质降级告警阈值配置与判定/展示函数"
```

---

### Task 3: Worker 记录画质 + 录制启动告警 + 断流复查刷新

**Files:**
- Modify: `crates/biliup-cli/src/server/infrastructure/context.rs:135-150`（Worker 加字段）、`:160-172`（new 初始化）、加 setter
- Modify: `crates/biliup-cli/src/server/common/download.rs:336-337`（启动后告警+写画质）、`:362-364`（结束清空）、`:192`（复查刷新）

**Interfaces:**
- Consumes: `LiveStream.recording_quality`（Task 1）、`quality_below_alert` / `quality_display`（Task 2）、`Config.douyin_quality_alert` / `cookie_health_webhook`。
- Produces: `Worker.recording_quality: RwLock<Option<String>>`，方法 `pub fn set_recording_quality(&self, q: Option<String>)`、`pub fn recording_quality(&self) -> Option<String>`。

- [ ] **Step 1: Worker 加字段与方法**

`context.rs` 的 `pub struct Worker { … }`（`:135-150`）加：
```rust
    /// 当前录制的实际画质代码（仅录制中有值，用于前端 tag）
    pub recording_quality: RwLock<Option<String>>,
```
`Worker::new`（`:160-172`）的 `Self { … }` 加：
```rust
            recording_quality: RwLock::new(None),
```
在 `impl Worker`（`get_config` 附近）加方法：
```rust
    pub fn set_recording_quality(&self, q: Option<String>) {
        *self.recording_quality.write().unwrap() = q;
    }

    pub fn recording_quality(&self) -> Option<String> {
        self.recording_quality.read().unwrap().clone()
    }
```

- [ ] **Step 2: 录制启动处告警 + 写画质（每场一次）**

`download.rs:start_download_workflow`，在 `ctx.change_status(... Working ...)` 之后（`:337` 后）插入：

```rust
    // 记录实际画质供前端 tag 显示
    let recording_quality = ctx.live_stream().recording_quality.clone();
    ctx.worker().set_recording_quality(recording_quality.clone());

    // 抖音画质降级告警：实际画质低于阈值则推送（每场开播仅此一次）
    if ctx.live_stream().platform == "douyin"
        && let Some(actual) = recording_quality.as_deref()
    {
        let cfg = ctx.config();
        if crate::server::common::cookie_health::quality_below_alert(
            actual,
            cfg.douyin_quality_alert.as_deref(),
        ) {
            let threshold = cfg.douyin_quality_alert.as_deref().unwrap_or("uhd");
            let threshold = if threshold.trim().is_empty() { "uhd" } else { threshold };
            let actual_disp = crate::server::common::cookie_health::quality_display(actual);
            let threshold_disp = crate::server::common::cookie_health::quality_display(threshold);
            crate::server::common::cookie_health::notify_alert(
                cfg.cookie_health_webhook.as_deref(),
                "⚠️ 抖音 未录到蓝光画质",
                &format!(
                    "{}：当前录制画质为 {}({})，低于告警阈值 {}({})，可能是 cookie（sessionid）失效，建议检查更换。",
                    ctx.live_streamer().url,
                    actual_disp, actual, threshold_disp, threshold,
                ),
            );
        }
    }
```

- [ ] **Step 3: 录制结束清空画质**

`download.rs:start_download_workflow` 末尾，`task.execute(...)` 之后（`:362` 后、`downloaded_processor` 之前或之后均可）加：
```rust
    ctx.worker().set_recording_quality(None);
```

- [ ] **Step 4: 断流复查刷新画质（不推送）**

`download.rs:192`，复查成功分支 `stream = *next_stream;` 之后加：
```rust
                    ctx.worker().set_recording_quality(stream.recording_quality.clone());
```

- [ ] **Step 5: 编译**

Run: `cargo build -p biliup-cli 2>&1 | tail -10`
Expected: 编译通过（`if let` 链式需确认仓库 edition 支持；本仓库已在 `douyin.rs:152`、`config.rs` 等处使用 `&& let`，故可用）。

- [ ] **Step 6: Commit**

```bash
git add crates/biliup-cli/src/server/infrastructure/context.rs crates/biliup-cli/src/server/common/download.rs
git commit -m "feat: 录制启动判定画质降级并推送，Worker 跟踪当前画质"
```

---

### Task 4: 接口返回实际画质

**Files:**
- Modify: `crates/biliup-cli/src/server/infrastructure/dto.rs:6-16`（响应加字段）
- Modify: `crates/biliup-cli/src/server/api/endpoints.rs:43-69`（填充字段）

**Interfaces:**
- Consumes: `Worker.recording_quality()`（Task 3）。
- Produces: `LiveStreamerResponse.recording_quality: Option<String>`（JSON 字段 `recording_quality`）。

- [ ] **Step 1: 响应结构加字段**

`dto.rs` 的 `LiveStreamerResponse` 加：
```rust
    /// 当前录制的实际画质代码（录制中才有值）
    pub recording_quality: Option<String>,
```

- [ ] **Step 2: endpoint 填充**

`endpoints.rs:56-67`，把 `recording_quality` 一并取出并放入响应：
```rust
        let recording_quality = option.as_ref().and_then(|t| t.recording_quality());

        results.push(LiveStreamerResponse {
            status,
            inner: x,
            upload_status: option
                .map(|t| format!("{:?}", *t.uploader_status.read().unwrap()))
                .unwrap_or_default(),
            recording_quality,
        });
```
（注意：`option` 在 `upload_status` 处被 `map` 消费。把 `recording_quality` 的提取放在 `upload_status` 之前用 `option.as_ref()`，或将 `option` 改为先 `as_ref()`。按上面顺序：先 `option.as_ref().and_then(...)` 求出 `recording_quality`，`upload_status` 内仍用 `option.map(...)`——因前者借用、后者消费，顺序需先借用后消费，故 `recording_quality` 行放在 `results.push` 之前、`status` 之后即可。）

- [ ] **Step 3: 编译**

Run: `cargo build -p biliup-cli 2>&1 | tail -10`
Expected: 编译通过。

- [ ] **Step 4: Commit**

```bash
git add crates/biliup-cli/src/server/infrastructure/dto.rs crates/biliup-cli/src/server/api/endpoints.rs
git commit -m "feat: /v1/streamers 返回 recording_quality"
```

---

### Task 5: 前端告警阈值配置项 + 录制画质 tag

**Files:**
- Modify: `app/ui/plugins/douyin.tsx:50`（画质下拉之后加告警阈值下拉）
- Modify: `app/(app)/streamers/page.tsx:57-92`（录制中追加画质 tag）

**Interfaces:**
- Consumes: 接口字段 `recording_quality`（Task 4）、表单 field `douyin_quality_alert`（Task 2 后端字段）。

- [ ] **Step 1: 加告警阈值下拉**

`douyin.tsx`，在画质 `Form.Select`（结束于 `:50` 的 `</Form.Select>`）之后插入：

```tsx
        <Form.Select
          field="douyin_quality_alert"
          extraText={
            <div style={{ fontSize: '14px' }}>
              实际录到的画质低于此档时，通过 cookie 健康 webhook 推送提醒（常见于 cookie 失效）。默认蓝光，可选「关闭通知」。
            </div>
          }
          label="画质降级告警阈值（douyin_quality_alert）"
          style={{ width: '100%' }}
          fieldStyle={{ alignSelf: 'stretch', padding: 0 }}
          showClear={true}
        >
          <Select.Option value="off">关闭通知</Select.Option>
          <Select.Option value="origin">原画（origin）</Select.Option>
          <Select.Option value="uhd">蓝光（uhd）</Select.Option>
          <Select.Option value="hd">超清（hd）</Select.Option>
          <Select.Option value="sd">高清（sd）</Select.Option>
          <Select.Option value="ld">标清（ld）</Select.Option>
          <Select.Option value="md">流畅（md）</Select.Option>
        </Form.Select>
```

- [ ] **Step 2: 录制中追加画质 tag**

`streamers/page.tsx`，在 `missingUpload` 定义之后（`:82` 后）加画质映射与 tag：

```tsx
    const qualityName: Record<string, string> = {
      origin: '原画', uhd: '蓝光', hd: '超清', sd: '高清', ld: '标清', md: '流畅',
    }
    const recordingTag =
      live.status === 'Working' && live.recording_quality ? (
        <Tag color="light-blue" style={{ marginLeft: 4 }}>
          {(qualityName[live.recording_quality] ?? live.recording_quality)} 录制中
        </Tag>
      ) : null
```

并把返回的 `statusTag` 片段（`:85-90`）改为：
```tsx
      statusTag: (
        <>
          {statusTag}
          {recordingTag}
          {missingUpload}
        </>
      ),
```

若 TypeScript 类型报 `recording_quality` 不存在，于该实体类型（`LiveStreamerEntity` 所在定义文件）加可选字段 `recording_quality?: string`。

- [ ] **Step 3: 前端构建校验**

Run: `npm run build 2>&1 | tail -20`
Expected: 构建通过，无类型错误。

- [ ] **Step 4: Commit**

```bash
git add app/ui/plugins/douyin.tsx "app/(app)/streamers/page.tsx"
git commit -m "feat(ui): 抖音画质降级告警阈值配置与录制画质 tag"
```

---

### Task 6: 全量构建与手动验收

**Files:** 无（验证）

- [ ] **Step 1: 后端全量测试**

Run: `cargo test -p biliup -p biliup-cli 2>&1 | tail -20`
Expected: 全绿，含 `quality_tests`、`quality_alert_tests`、既有 `alert_tests`。

- [ ] **Step 2: 手动验收清单（人工）**

- 空间配置 → 抖音：出现「画质降级告警阈值」下拉，默认空（=蓝光）；可保存。
- 直播管理 → 单房间编辑 → 抖音：同样出现该下拉，单独保存生效。
- 录一个抖音房间：录制中房间旁出现「<画质> 录制中」tag；实际低于蓝光时收到 webhook（标题「⚠️ 抖音 未录到蓝光画质」）。
- 阈值设为「关闭通知」时不再推送；恢复蓝光后录到蓝光/原画不推送。

- [ ] **Step 3: Commit（如有验收期间的微调）**

```bash
git add -A && git commit -m "chore: 抖音画质降级告警验收微调"
```

---

## Self-Review

- **Spec 覆盖**：Part1 阈值配置=Task2(后端)+Task5(前端)；判定+推送=Task2+Task3；Part2 暴露实际画质=Task1；Part3 tag=Task3(Worker)+Task4(接口)+Task5(前端)；测试=Task1/2 单测+Task6。双层配置由 `#[derive(Patch)]` 自动覆盖（无需独立任务，已在 Global Constraints 与 Task2 说明）。
- **占位符**：无 TBD/TODO；每个代码步骤含完整代码。
- **类型一致性**：`recording_quality: Option<String>` 在 LiveStream/Worker(字段为 `RwLock<Option<String>>`，方法返回 `Option<String>`)/响应一致；`quality_below_alert(&str, Option<&str>)->bool`、`quality_display(&str)->String`、`select_quality_code(&[&str],&str)->Option<&'static str>` 全程一致。
- **已知取舍**：`quality_display` 返回 `String`（非 `&'static str`）以支持未知码原样返回；Task2 Step3 已用最终版替换草稿，测试断言 `String == &str` 合法。
