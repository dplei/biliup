# `scripts/` —— 仓库自带的工具

| 脚本 | 做什么 | 什么时候用 |
| --- | --- | --- |
| [`dev.sh`](dev.sh) | 起本机开发环境：按需构建前端与后端，绑 `127.0.0.1:19159` | 本地跑服务、手工验收 |
| [`check_code_index.py`](check_code_index.py) | 校验 `CODE_INDEX.md` 的路径、重复条目和悬空关系 | 改完 `CODE_INDEX.md` 之后 |
| [`consistency-audit.sh`](consistency-audit.sh) | 只读巡检：找出投稿与本地账本之间的错位 | 怀疑稿件重复/缺分P、或想确认某类问题是孤例还是系统性 |

---

## `dev.sh`

```bash
scripts/dev.sh            # 只起后端（前端走 out/ 里已构建的静态页）
scripts/dev.sh --web      # 后端 + Next.js 热重载
scripts/dev.sh --release  # release 档编译后端，贴近镜像里的性能表现
```

必须从仓库根目录运行——后端固定读写 cwd 下的 `data/data.sqlite3`。

## `check_code_index.py`

```bash
python3 scripts/check_code_index.py
```

改过 `CODE_INDEX.md` 就跑一次。约定见 [`docs/agents/code-index.md`](../docs/agents/code-index.md)。

## `consistency-audit.sh`

```bash
DB=<数据库路径> bash scripts/consistency-audit.sh
DB=<数据库路径> bash scripts/consistency-audit.sh > audit-$(date +%F).txt   # 存档以便日后比对
```

全程 `sqlite3 -readonly` + `PRAGMA query_only=ON`，不写库、不重启服务，正在直播时也能跑，
可以挂 cron 定期跑并 diff 结果。不给 `DB` 时只在仓库内找 dev 库；核对生产库必须显式传 `DB=`。

输出五节：错位会话总览、干净会话对照组、重复条目明细、`video_json` 回填率、`last_error` 形态。
按会话的 `submit_requested_at` 是否为空切成 `legacy` / `current` 两组，可以直接看出某类问题
是历史遗留还是仍在发生。

### 读这份输出前必须知道的两件事

**1. `upload_missing_segment` 是补救账本，不是全量分段账本。**

只有需要补救的分段才会进这张表；走正常上传路径的会话在里面**一行都没有**。所以：

- 「某会话在这张表里没有行」是**健康**状态，不是异常；
- 单条补救记录的 `segment_order` 反映的是它在**整场直播**里的位置，不是它在账本里的位置，
  因此序号天然稀疏，「序号不连续」对这张表不成立；
- 历史数据的 `segment_order` 基准从 1-based 换过 0-based，跨基准比较必然差一。

这个脚本的第一版就是把它当成了全量账本，结果把「没有补救记录」判成异常，误报率高达八成以上。
**给这张表写任何查询之前，先确认你要的是全量视角还是补救视角。**

**2. 稿件侧和分段侧各有一份远端记录，且历史数据只有一侧可用。**

- `upload_session.videos_json` —— 会话级的稿件分P清单，一直都有；
- `upload_missing_segment.video_json` —— 分段级的远端返回，**新逻辑生效后才回填**，
  更早的历史会话大多为空。

所以「从分段行侧交叉验证稿件」这条路只对新数据成立。第 4 节专门给出回填率，
先看它再决定第 3 节的结论能信到什么程度。

### 相关

`last_error` 目前同时承载「当前失败原因」「历史失败残留」「成功路径的状态说明」三种语义，
重试成功后不清空，**不能**用「`last_error` 非空」判健康——见
[dplei/biliup#7](https://github.com/dplei/biliup/issues/7)，第 5 节量化了它的影响面。
