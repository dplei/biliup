#!/usr/bin/env python3
"""算出把坏掉的时间戳「平移」接回去所需的 setts 参数，并打印现成的 ffmpeg 命令。

两种形态，都按 DTS 判定（PTS 在含 B 帧的流里按解码序本来就不单调，拿它判会满屏假回退）：

- **回退**：时间戳往回跳。把回退点之后整体加偏移，时间轴接着往下走，内容一帧不丢，
  只是文件变长。每条流各算各的。服务端早先用 `max(TS, PREV_OUT+1)` 夹取，回退量大时
  后面的内容会被压成帧风暴；现已改成按流累加增量（`timestamp_repair.rs` 的 `delta_setts`），
  效果与这里相当，修不好的留在本地等这个脚本。
- **前跳**：直播源断流，相邻包向前跳几十秒，中间没有数据。B 站会以「时间戳跳变」拒绝转码，
  而 ffmpeg 的 dts 回退扫描对它 0 命中。把空洞之后整体往回收；**所有流必须在同一处一起跳**，
  并且共用同一个偏移，否则会引入音画漂移。偏移还要给 B 帧留余量：断流截断的 GOP，空洞前
  最大 PTS 可能比它的 DTS 晚一两帧，收回后空洞后的首帧 PTS 必须仍在它之后。

    python3 scripts/timestamp_shift.py 坏文件.flv [产物]

产物默认与源同一容器（flv→mp4 的 edit list 会让解码少出帧）。两种形态都没有、
或者两种混在一起、或者只有部分流前跳，都直说，不硬套。
"""

import json
import shlex
import subprocess
import sys
from fractions import Fraction
from pathlib import Path
from statistics import median

GAP_SECONDS = 1.0


def probe_packets(path):
    """{流序号: (codec_type, time_base, [(dts, pts), ...])}，时间戳单位是该流的 time_base。"""
    out = subprocess.run(
        [
            "ffprobe", "-v", "error",
            "-show_entries", "packet=stream_index,pts,dts",
            "-show_entries", "stream=index,codec_type,time_base",
            "-of", "json", path,
        ],
        capture_output=True, text=True, check=True,
    ).stdout
    data = json.loads(out)
    streams = {
        s["index"]: (s.get("codec_type", "?"), Fraction(s["time_base"]), [])
        for s in data.get("streams", [])
    }
    for packet in data.get("packets", []):
        pts, dts = packet.get("pts"), packet.get("dts")
        if dts is None:
            dts = pts
        if dts is None:
            continue
        streams[packet["stream_index"]][2].append((int(dts), int(pts if pts is not None else dts)))
    return {index: s for index, s in streams.items() if s[2]}


def typical_delta(dts, limit):
    """相邻 DTS 的中位差，用来把偏移补成「接着下一帧」而不是原地重合。"""
    deltas = [b - a for a, b in zip(dts, dts[1:]) if 0 < b - a < limit]
    return int(median(deltas)) if deltas else 1


def find_drops(dts, delta):
    """所有回退点：(流内包序号, 需要补的偏移量)。偏移是累加的，多次回退各记各的。"""
    drops = []
    shift = 0
    for index in range(1, len(dts)):
        if dts[index] + shift < dts[index - 1] + shift:
            offset = dts[index - 1] - dts[index] + delta
            drops.append((index, offset))
            shift += offset
    return drops


def find_gaps(dts, limit):
    """所有前跳：(空洞前 DTS, 空洞后 DTS)。"""
    return [(a, b) for a, b in zip(dts, dts[1:]) if b - a > limit]


def plan_forward(streams):
    """把各流的前跳聚成断流点，返回 [(阈值秒, 累计收回秒)]；做不了就抛 ValueError 说明原因。"""
    gaps = []  # (前秒, 后秒, 流序号)
    for index, (_, tb, packets) in streams.items():
        dts = [d for d, _ in packets]
        gaps += [(a * tb, b * tb, index) for a, b in find_gaps(dts, GAP_SECONDS / tb)]
    gaps.sort()

    clusters = []
    for gap in gaps:
        if clusters and gap[0] < min(g[1] for g in clusters[-1]):
            clusters[-1].append(gap)
        else:
            clusters.append([gap])

    plan = []
    total = Fraction(0)
    for cluster in clusters:
        lo, hi = max(g[0] for g in cluster), min(g[1] for g in cluster)
        members = sorted(g[2] for g in cluster)
        if members != sorted(streams) or lo >= hi:
            raise ValueError(
                f"约 {float(cluster[0][0]):.3f}s 处只有流 {members} 前跳，"
                "不是整体断流，收回去会音画漂移"
            )
        threshold = (lo + hi) / 2
        room = []
        frames = []
        for _, tb, packets in streams.values():
            pre = [(d * tb, p * tb) for d, p in packets if d * tb < threshold]
            post = [(d * tb, p * tb) for d, p in packets if d * tb >= threshold]
            room.append(min(d for d, _ in post) - max(d for d, _ in pre))
            room.append(min(p for _, p in post) - max(p for _, p in pre))
            frames.append(typical_delta([d for d, _ in packets], GAP_SECONDS / tb) * tb)
        offset = min(room) - max(frames)
        if offset <= 0:
            raise ValueError(f"约 {float(threshold):.3f}s 处的空洞比 B 帧余量还小，不需要也不能收")
        total += offset
        plan.append((threshold, total))
    return plan


def forward_expression(plan, tb):
    expr = "0"
    for threshold, total in plan:
        expr = f"if(gte(DTS\\,{round(threshold / tb)})\\,{round(total / tb)}\\,{expr})"
    return f"setts=pts=PTS-{expr}:dts=DTS-{expr}"


def drop_expression(drops):
    terms = "".join(f"+if(gte(N\\,{index})\\,{offset}\\,0)" for index, offset in drops)
    return f"setts=pts=PTS{terms}:dts=DTS{terms}"


def build(source, output):
    """返回 (报告行, ffmpeg 命令或 None)。"""
    streams = probe_packets(source)
    report = []
    expressions = {}

    drops_by_stream = {}
    for index, (kind, tb, packets) in sorted(streams.items()):
        dts = [d for d, _ in packets]
        drops = find_drops(dts, typical_delta(dts, GAP_SECONDS / tb))
        if drops:
            drops_by_stream[index] = drops
            report += [f"流 {index}（{kind}）：第 {i} 个包处回退，补偏移 {o}" for i, o in drops]
        else:
            report.append(f"流 {index}（{kind}）：{len(packets)} 个包，没有回退")

    try:
        plan = plan_forward(streams)
    except ValueError as error:
        return report + [f"前跳：{error}"], None
    for threshold, total in plan:
        report.append(f"前跳：{float(threshold):.3f}s 之后整体收回到累计 {float(total):.3f}s")

    if drops_by_stream and plan:
        return report + ["回退与前跳混在一起，这个脚本不处理"], None
    if not drops_by_stream and not plan:
        return report + ["没有回退也没有前跳，不需要平移"], None

    for index, (_, tb, _) in streams.items():
        if plan:
            expressions[index] = forward_expression(plan, tb)
        elif index in drops_by_stream:
            expressions[index] = drop_expression(drops_by_stream[index])

    is_mp4 = Path(output).suffix.lower() in (".mp4", ".mov", ".m4v")
    args = []
    for index, expr in sorted(expressions.items()):
        kind = streams[index][0]
        if kind == "audio" and is_mp4:
            expr = f"aac_adtstoasc,{expr}"
        if kind in ("audio", "video"):
            args += [f"-bsf:{kind[0]}", expr]
        else:
            report.append(f"！流 {index} 是 {kind}，这个脚本不处理，产物里它的时间戳不变")

    command = [
        "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
        # -copyts：ts 这类格式 ffmpeg 会在 bsf 之前自己纠正不连续，再平移一次就成了双倍
        "-fflags", "+genpts", "-copyts", "-i", source, "-c", "copy", *args,
        *(["-movflags", "+faststart"] if is_mp4 else []),
        "-avoid_negative_ts", "make_zero",
        "-muxdelay", "0", "-muxpreload", "0",
        output,
    ]
    return report, command


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    source = sys.argv[1]
    output = sys.argv[2] if len(sys.argv) > 2 else "已修复" + Path(source).suffix
    report, command = build(source, output)
    print("\n".join(report))
    if command:
        print("\n跑这条：\n")
        print(" ".join(shlex.quote(part) for part in command))
        print("\n跑完必须复检：扫描零命中、包数逐流不变、时间轴单调、音画末差不变。")


if __name__ == "__main__":
    main()
