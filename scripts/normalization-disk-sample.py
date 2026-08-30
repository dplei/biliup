#!/usr/bin/env python3
"""响度标准化的磁盘峰值采样器。

按固定间隔盯住录像目录，记录标准化中间件（`*.audio-normalized-*.part.*`）的数量与
字节总和，退出时给出峰值摘要和一句判定。这是 `.scratch/audio-normalization-disk-budget/`
的 06 号验收工具：就地替换要成立，同一时刻最多只能有一份中间件。

    # 实验组：默认配置（就地替换），断言任何采样点上中间件都不超过一份
    python3 scripts/normalization-disk-sample.py data/recordings --csv in-place.csv

    # 对照组：audio_normalization_keep_original: true，预期能看到多份并存
    python3 scripts/normalization-disk-sample.py data/recordings --csv keep-original.csv \
        --max-parts 99

跑到你觉得够了就 Ctrl-C，摘要打在 stderr、逐点数据写进 --csv。判定不通过时退出码为 1。

只读：不碰录像文件，也不连数据库，正在直播时可以跑。
"""

from __future__ import annotations

import argparse
import csv
import os
import signal
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

# 与 `normalized_temp_path` 的命名一致；改那边的话这里要跟着改。
ARTIFACT_MARK = ".audio-normalized-"
PART_MARK = ".part."


@dataclass
class Sample:
    at: float
    parts: int
    part_bytes: int
    total_bytes: int
    recordings: int


@dataclass
class Peaks:
    parts: int = 0
    part_bytes: int = 0
    total_bytes: int = 0
    at_parts: float = 0.0
    at_part_bytes: float = 0.0
    samples: list[Sample] = field(default_factory=list)

    def observe(self, sample: Sample) -> None:
        # 第一个样本无条件占位，否则峰值恰好是 0 时时刻会停在 epoch 0。
        first = not self.samples
        self.samples.append(sample)
        if first or sample.parts > self.parts:
            self.parts = sample.parts
            self.at_parts = sample.at
        if first or sample.part_bytes > self.part_bytes:
            self.part_bytes = sample.part_bytes
            self.at_part_bytes = sample.at
        self.total_bytes = max(self.total_bytes, sample.total_bytes)


def is_artifact(name: str) -> bool:
    return ARTIFACT_MARK in name and PART_MARK in name


def scan(directory: Path) -> Sample:
    """扫一层目录。录像都落在同一层，不递归是为了让每次采样足够便宜。"""
    parts = 0
    part_bytes = 0
    total_bytes = 0
    recordings = 0
    with os.scandir(directory) as entries:
        for entry in entries:
            if not entry.is_file(follow_symlinks=False):
                continue
            try:
                size = entry.stat(follow_symlinks=False).st_size
            except OSError:
                # 采样期间文件被删掉是正常的，跳过即可。
                continue
            total_bytes += size
            if is_artifact(entry.name):
                parts += 1
                part_bytes += size
            else:
                recordings += 1
    return Sample(time.time(), parts, part_bytes, total_bytes, recordings)


def human(value: int) -> str:
    for unit in ("B", "KiB", "MiB", "GiB"):
        if value < 1024 or unit == "GiB":
            return f"{value:.0f} {unit}" if unit == "B" else f"{value:.2f} {unit}"
        value /= 1024
    return f"{value:.2f} GiB"


def main() -> int:
    parser = argparse.ArgumentParser(
        description="采样响度标准化中间件占用的磁盘峰值",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("directory", type=Path, help="录像所在目录")
    parser.add_argument(
        "--interval", type=float, default=1.0, help="采样间隔（秒），默认 1"
    )
    parser.add_argument(
        "--duration",
        type=float,
        default=0.0,
        help="采样时长（秒）；默认 0 表示一直跑到 Ctrl-C",
    )
    parser.add_argument("--csv", type=Path, help="把逐点数据写到这个文件")
    parser.add_argument(
        "--max-parts",
        type=int,
        default=1,
        help="允许的中间件数量上限，超过即判定失败。就地替换应为 1；"
        "对照组（keep_original）用一个大值让它只采样不判定",
    )
    args = parser.parse_args()

    if not args.directory.is_dir():
        print(f"不是一个目录：{args.directory}", file=sys.stderr)
        return 2

    peaks = Peaks()
    stopping = False

    def stop(_signum, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)

    started = time.time()
    print(
        f"采样 {args.directory}，间隔 {args.interval}s，Ctrl-C 结束",
        file=sys.stderr,
    )
    while not stopping:
        peaks.observe(scan(args.directory))
        if args.duration and time.time() - started >= args.duration:
            break
        time.sleep(args.interval)

    if not peaks.samples:
        print("一个采样点都没有", file=sys.stderr)
        return 2

    if args.csv:
        with args.csv.open("w", newline="") as handle:
            writer = csv.writer(handle)
            writer.writerow(
                ["epoch", "elapsed_s", "parts", "part_bytes", "total_bytes", "recordings"]
            )
            for sample in peaks.samples:
                writer.writerow(
                    [
                        f"{sample.at:.3f}",
                        f"{sample.at - started:.3f}",
                        sample.parts,
                        sample.part_bytes,
                        sample.total_bytes,
                        sample.recordings,
                    ]
                )
        print(f"逐点数据：{args.csv}", file=sys.stderr)

    elapsed = peaks.samples[-1].at - started
    print(f"\n采样点 {len(peaks.samples)} 个，历时 {elapsed:.0f}s", file=sys.stderr)
    print(
        f"中间件数量峰值：{peaks.parts}（第 {peaks.at_parts - started:.0f}s）",
        file=sys.stderr,
    )
    print(
        f"中间件字节峰值：{human(peaks.part_bytes)}"
        f"（第 {peaks.at_part_bytes - started:.0f}s）",
        file=sys.stderr,
    )
    print(f"目录占用峰值：{human(peaks.total_bytes)}", file=sys.stderr)

    if peaks.parts > args.max_parts:
        print(
            f"\n判定：不通过——中间件最多同时存在 {peaks.parts} 份，超过上限 "
            f"{args.max_parts}。就地替换没有生效，或者有 .part 没被回收。",
            file=sys.stderr,
        )
        return 1
    if peaks.parts == 0:
        print(
            "\n判定：没采到任何中间件。要么这段时间没有分段完成，要么标准化没开——"
            "先确认 audio_normalization_enabled，再重跑。",
            file=sys.stderr,
        )
        return 1
    print(
        f"\n判定：通过——中间件任何时刻不超过 {peaks.parts} 份，"
        f"额外占用峰值 {human(peaks.part_bytes)}。",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
