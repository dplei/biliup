#!/usr/bin/env python3
"""算出把回退的时间戳「平移」接回去所需的 setts 参数，并打印现成的 ffmpeg 命令。

服务端的修复用 `setts=...max(TS, PREV_OUT+1)` 把回退那段时间**夹掉**，代价是回退量一旦
接近剩余内容时长，后面的内容会被压成帧风暴，所以那边有 10 秒的闸门（见
`crates/biliup-cli/src/server/common/timestamp_repair.rs`）。

本机不受那个限制，可以做更正确的修法：**把回退点之后的时间戳整体加一个偏移**，让时间轴
接着往下走。内容一帧不丢，只是文件变长。这个脚本负责算出每条流的「从第几个包开始加、
加多少」。

    python3 scripts/timestamp_shift.py 坏文件.flv [产物.mp4]

没有回退就直说，不要硬套。
"""

import json
import shlex
import subprocess
import sys
from statistics import median


def probe_packets(path):
    out = subprocess.run(
        [
            "ffprobe", "-v", "error",
            "-show_entries", "packet=stream_index,pts",
            "-show_entries", "stream=index,codec_type",
            "-of", "json", path,
        ],
        capture_output=True, text=True, check=True,
    ).stdout
    data = json.loads(out)
    kinds = {s["index"]: s.get("codec_type", "?") for s in data.get("streams", [])}
    streams = {}
    for packet in data.get("packets", []):
        pts = packet.get("pts")
        if pts is None:
            continue
        streams.setdefault(packet["stream_index"], []).append(int(pts))
    return kinds, streams


def typical_delta(timestamps):
    """相邻时间戳的中位差，用来把偏移量补成「接着下一帧」而不是原地重合。"""
    deltas = [b - a for a, b in zip(timestamps, timestamps[1:]) if 0 < b - a < 10_000]
    return int(median(deltas)) if deltas else 1


def find_drops(timestamps):
    """所有回退点：(流内包序号, 需要补的偏移量)。偏移是累加的，多次回退各记各的。"""
    delta = typical_delta(timestamps)
    drops = []
    shift = 0
    for index in range(1, len(timestamps)):
        current = timestamps[index] + shift
        previous = timestamps[index - 1] + shift
        if current < previous:
            offset = previous - current + delta
            drops.append((index, offset))
            shift += offset
    return drops, delta


def expression(drops):
    terms = "".join(f"+if(gte(N\\,{index})\\,{offset}\\,0)" for index, offset in drops)
    return f"setts=pts=PTS{terms}:dts=DTS{terms}"


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    source = sys.argv[1]
    output = sys.argv[2] if len(sys.argv) > 2 else "已修复.mp4"

    kinds, streams = probe_packets(source)
    args = []
    clean = True
    for stream_index in sorted(streams):
        timestamps = streams[stream_index]
        drops, delta = find_drops(timestamps)
        kind = kinds.get(stream_index, "?")
        if not drops:
            print(f"流 {stream_index}（{kind}）：{len(timestamps)} 个包，没有回退")
            continue
        clean = False
        for index, offset in drops:
            print(
                f"流 {stream_index}（{kind}）：第 {index} 个包处回退，"
                f"补偏移 {offset}（典型帧间隔 {delta}）"
            )
        expr = expression(drops)
        if kind == "audio":
            args += ["-bsf:a", f"aac_adtstoasc,{expr}"]
        elif kind == "video":
            args += ["-bsf:v", expr]
        else:
            print(f"  ！流 {stream_index} 是 {kind}，这个脚本不处理，产物里它的时间戳不变")

    if clean:
        print("\n没有检测到时间戳回退，不需要平移。")
        return

    command = [
        "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
        "-fflags", "+genpts", "-i", source, "-c", "copy",
        *args,
        "-movflags", "+faststart",
        "-avoid_negative_ts", "make_zero",
        "-muxdelay", "0", "-muxpreload", "0",
        output,
    ]
    print("\n跑这条：\n")
    print(" ".join(shlex.quote(part) for part in command))
    print("\n跑完必须复检：包数不变、扫描零命中、时间轴单调。")


if __name__ == "__main__":
    main()
