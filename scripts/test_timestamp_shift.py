"""Run with: python3 -m unittest discover -s scripts -p 'test_timestamp_shift.py'.

需要带 libx264 的 ffmpeg。也可以直接复检一对真实文件：
    python3 scripts/test_timestamp_shift.py 原片.flv 修好的.flv
"""

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from timestamp_shift import build, probe_packets


def recheck(source, fixed):
    """复检四项所需的数字。每条流按 codec_type 对齐（换容器后流序号可能互换）。"""
    scan = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "verbose", "-nostats", "-fflags", "+igndts",
         "-i", fixed, "-c", "copy", "-f", "null", "-"],
        capture_output=True, text=True,
    ).stderr.count("non monotonically increasing dts")

    def decode_errors(path):
        return subprocess.run(
            ["ffmpeg", "-hide_banner", "-v", "error", "-i", path, "-f", "null", "-"],
            capture_output=True, text=True,
        ).stderr.count("\n")

    frames = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries", "frame=pts",
         "-of", "csv=p=0", fixed],
        capture_output=True, text=True, check=True,
    ).stdout.split()
    frames = [int(f) for f in frames if f.lstrip("-").isdigit()]

    def summary(path):
        out = {}
        for kind, tb, packets in probe_packets(path).values():
            dts = [d * tb for d, _ in packets]
            out[kind] = {
                "packets": len(packets),
                "cto": sum(d != p for d, p in packets),
                "monotonic": all(a < b for a, b in zip(dts, dts[1:])),
                "max_gap": max(b - a for a, b in zip(dts, dts[1:])),
                "end_pts": packets[-1][1] * tb,
            }
        return out

    src, out = summary(source), summary(fixed)
    return {
        "scan_hits": scan,
        "new_decode_errors": decode_errors(fixed) - decode_errors(source),
        "frame_order_monotonic": all(a < b for a, b in zip(frames, frames[1:])),
        "src": src,
        "out": out,
        "av_end_drift": abs(
            (out["video"]["end_pts"] - out["audio"]["end_pts"])
            - (src["video"]["end_pts"] - src["audio"]["end_pts"])
        ),
    }


def run(command):
    subprocess.run(command, check=True, capture_output=True)


@unittest.skipUnless(shutil.which("ffmpeg"), "需要 ffmpeg")
class ForwardJumpTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)

    def make(self, video_expr, audio_expr, drop="0"):
        """合成 12 秒、带 B 帧的 flv，再用 noise 丢包、setts 人为造出时间戳形态。"""
        clean = self.dir / "clean.flv"
        run(["ffmpeg", "-y", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=60",
             "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "12",
             "-c:v", "libx264", "-bf", "3", "-g", "60", "-c:a", "aac", str(clean)])
        broken = self.dir / "broken.flv"
        run(["ffmpeg", "-y", "-i", str(clean), "-c", "copy",
             "-bsf:v", f"noise=drop={drop},setts=pts=PTS+{video_expr}:dts=DTS+{video_expr}",
             "-bsf:a", f"setts=pts=PTS+{audio_expr}:dts=DTS+{audio_expr}", str(broken)])
        return str(broken)

    def test_two_forward_gaps_are_closed(self):
        # 4 秒和 8 秒处各断流约 19 秒，两条流一起跳，音频空洞略大（与真实样本同形）。
        # 断流前 200ms 只留 composition offset 大的 P 帧：GOP 被截断，空洞前最大 PTS
        # 比 DTS 晚好几帧。偏移若只看 DTS，空洞后首帧会排到它前面，解码输出序乱掉。
        video = "if(gte(DTS\\,8000)\\,38000\\,if(gte(DTS\\,4000)\\,19000\\,0))"
        audio = "if(gte(DTS\\,8000)\\,38100\\,if(gte(DTS\\,4000)\\,19100\\,0))"
        drop = "+".join(
            f"between(dts\\,{t - 200}\\,{t - 1})*lt(pts-dts\\,60)" for t in (4000, 8000)
        )
        source = self.make(video, audio, drop)
        fixed = str(self.dir / "fixed.flv")
        report, command = build(source, fixed)
        self.assertIsNotNone(command, report)
        run(command)

        result = recheck(source, fixed)
        self.assertEqual(result["scan_hits"], 0)
        self.assertEqual(result["new_decode_errors"], 0)
        self.assertTrue(result["frame_order_monotonic"])
        for kind in ("video", "audio"):
            src, out = result["src"][kind], result["out"][kind]
            self.assertEqual(out["packets"], src["packets"], kind)
            self.assertEqual(out["cto"], src["cto"], kind)
            self.assertTrue(out["monotonic"], kind)
            self.assertGreater(src["max_gap"], 18)
            self.assertLess(out["max_gap"], 0.2, kind)
        self.assertGreater(result["src"]["video"]["cto"], 0, "样本里应当有 B 帧")
        self.assertLess(result["av_end_drift"], 0.05)

    def test_regression_is_shifted_forward(self):
        # muxer 会自己修掉回退，造不出来；照真实来源拼两段 ts，后一段时间戳早 3 秒
        parts = []
        for name, offset in (("a.ts", []), ("b.ts", ["-output_ts_offset", "3"])):
            run(["ffmpeg", "-y", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=60",
                 "-f", "lavfi", "-i", "sine=sample_rate=48000", "-t", "6",
                 "-c:v", "libx264", "-bf", "3", "-g", "60", "-c:a", "aac", *offset,
                 str(self.dir / name)])
            parts.append((self.dir / name).read_bytes())
        source = str(self.dir / "broken.ts")
        Path(source).write_bytes(b"".join(parts))
        fixed = str(self.dir / "fixed.ts")
        report, command = build(source, fixed)
        self.assertIsNotNone(command, report)
        run(command)
        result = recheck(source, fixed)
        self.assertEqual(result["scan_hits"], 0)
        self.assertTrue(result["frame_order_monotonic"])
        for kind in ("video", "audio"):
            src, out = result["src"][kind], result["out"][kind]
            self.assertFalse(src["monotonic"], kind)
            self.assertEqual(out["packets"], src["packets"], kind)
            self.assertTrue(out["monotonic"], kind)
            self.assertLess(out["max_gap"], 0.1, kind)
        self.assertLess(result["av_end_drift"], 0.05)

    def test_only_one_stream_jumping_is_refused(self):
        source = self.make("if(gte(DTS\\,4000)\\,19000\\,0)", "0")
        report, command = build(source, str(self.dir / "fixed.flv"))
        self.assertIsNone(command)
        self.assertIn("不是整体断流", "\n".join(report))

    def test_b_frames_alone_are_not_regressions(self):
        source = self.make("0", "0")
        report, command = build(source, str(self.dir / "fixed.flv"))
        self.assertIsNone(command, report)
        self.assertIn("没有回退也没有前跳", "\n".join(report))


if __name__ == "__main__":
    if len(sys.argv) == 3:
        for key, value in recheck(sys.argv[1], sys.argv[2]).items():
            print(key, value)
    else:
        unittest.main()
