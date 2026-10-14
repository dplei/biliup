# 01 前跳处理与 DTS 判定

Status: ready-for-human（PR 待合并）

- 改动：`scripts/timestamp_shift.py`；新增 `scripts/test_timestamp_shift.py`；
  `segment-recover` skill 第 2、3 步（`.claude/` 与 `.agents/` 两份）；`docs/code-index/maintenance.md`。
- 验证：
  - `python3 -m unittest discover -s scripts -p 'test_timestamp_shift.py'` 4 项全过：两处断流且 GOP 截断、
    单流前跳拒绝、纯 B 帧不误判、ts 拼接回退。
  - 变异检查：去掉 PTS 余量约束，断流用例失败（解码输出序乱）。
  - 旧脚本对完好的 B 帧样本报 172 处回退，新脚本判干净。
  - issue 中的真实样本：脚本算出 19.246 / 38.558 s（手工 19.250 / 38.550 s），复检扫描 0、
    新增解码错误 0、输出序单调、包数与 B 帧 offset 不变、音画末差漂移 0。
- 验收：本脚本只在本机应急使用，不进镜像；合并后下次 `segment-recover` 遇到前跳时直接用脚本
  产物通过复检即可关闭 issue。
