#!/usr/bin/env python3
"""Opus 帧编解码工具（模拟器用）

只依赖系统 Python 标准库 + ffmpeg。

用途：
1. WAV → 60ms 的 Opus 帧列表（模拟设备上行音频）
2. Opus 帧列表 → Ogg Opus 文件（验证下行音频能不能解码）

为什么不用 Python 的 opus 绑定：
- 环境里没有 opuslib，也不一定有 libopus 的 Python 绑定的编译环境
- ffmpeg 已经具备 libopus 编解码能力，且到处都有
- 我们只需要「编码一次 + 解码验证」，不需要实时性

小智协议的音频参数（来自 docs/mqtt-udp_zh.md）：
- 上行（设备→服务器）：16kHz 单声道，帧长 60ms → 每帧 960 采样
- 下行（服务器→设备）：24kHz 单声道，帧长 60ms → 每帧 1440 采样

注意：Opus 编码器的内部采样率固定为 48kHz，16k/24k 是 Opus 支持的低采样率模式。
ffmpeg 的 libopus 会自动重采样，我们只关心「每帧 60ms」这个约束。
"""

from __future__ import annotations

import os
import shutil
import struct
import subprocess
import tempfile
from dataclasses import dataclass


class OpusToolError(RuntimeError):
    """ffmpeg 调用失败或输入不合法"""


@dataclass
class OpusFrame:
    """一个 Opus 数据包"""

    data: bytes
    """裸 Opus 包（不含 Ogg 封装）"""

    @property
    def size(self) -> int:
        return len(self.data)


def _ffmpeg() -> str:
    exe = shutil.which("ffmpeg")
    if not exe:
        raise OpusToolError(
            "找不到 ffmpeg。模拟器需要它做 Opus 编解码。\n"
            "  macOS: brew install ffmpeg"
        )
    return exe


# ─────────────────────────────────────────────────────────────
# 编码：WAV → Opus 帧
# ─────────────────────────────────────────────────────────────


def wav_to_opus_frames(wav_path: str, sample_rate: int = 16000,
                       frame_duration_ms: int = 60,
                       bitrate: int = 24000) -> list[OpusFrame]:
    """把 WAV 文件编码成 60ms 一帧的 Opus 包列表。

    Args:
        wav_path: 输入 WAV（任意采样率，ffmpeg 会重采样）
        sample_rate: 目标采样率，上行用 16000
        frame_duration_ms: 帧长，小智协议固定 60
        bitrate: 码率，24000 对应 60ms/帧约 180 字节

    Returns:
        Opus 帧列表，顺序与时间顺序一致
    """
    if not os.path.isfile(wav_path):
        raise OpusToolError(f"WAV 文件不存在: {wav_path}")

    with tempfile.TemporaryDirectory() as tmp:
        ogg_path = os.path.join(tmp, "out.ogg")
        cmd = [
            _ffmpeg(), "-hide_banner", "-loglevel", "error", "-y",
            "-i", wav_path,
            "-ac", "1",
            "-ar", str(sample_rate),
            "-c:a", "libopus",
            "-b:a", str(bitrate),
            "-frame_duration", str(frame_duration_ms),
            # 关掉 VBR，保证每帧时长严格等于 frame_duration
            "-vbr", "off",
            "-application", "voip",
            ogg_path,
        ]
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            raise OpusToolError(
                f"ffmpeg 编码失败（exit {proc.returncode}）:\n{proc.stderr.strip()}"
            )
        if not os.path.isfile(ogg_path):
            raise OpusToolError("ffmpeg 没产出 Ogg 文件")
        return extract_ogg_packets(ogg_path)


def extract_ogg_packets(ogg_path: str) -> list[OpusFrame]:
    """从 Ogg 容器里抽出所有 Opus 数据包。

    Ogg 页结构（RFC 3533）：
        "OggS"            4 字节 捕获模式
        version           1 字节
        header_type       1 字节
        granule_position  8 字节
        serial_number     4 字节
        page_sequence     4 字节
        checksum          4 字节
        page_segments     1 字节
        segment_table     page_segments 字节
        payload           sum(segment_table) 字节

    页内的包按 segment_table 切分：长度 255 的段表示"包还没结束，续到下个段"。

    前两个包是 OpusHead / OpusTags（标识与注释），要跳过 —— 它们不是音频帧。
    """
    with open(ogg_path, "rb") as f:
        buf = f.read()

    packets: list[bytes] = []
    cur = bytearray()
    pos = 0
    n = len(buf)

    while pos + 27 <= n:
        if buf[pos:pos + 4] != b"OggS":
            # 找不到下一页，说明数据不合法或已到末尾
            break
        page_segments = buf[pos + 26]
        seg_table_start = pos + 27
        seg_table_end = seg_table_start + page_segments
        if seg_table_end > n:
            break
        seg_table = buf[seg_table_start:seg_table_end]
        payload_len = sum(seg_table)
        payload_start = seg_table_end
        payload_end = payload_start + payload_len
        if payload_end > n:
            break

        # 按 segment table 切包
        off = payload_start
        for seg_len in seg_table:
            cur += buf[off:off + seg_len]
            off += seg_len
            if seg_len < 255:
                # 包结束
                packets.append(bytes(cur))
                cur = bytearray()

        pos = payload_end

    if cur:
        # 收尾：最后一页可能以 255 结尾（理论上不该出现在我们场景）
        packets.append(bytes(cur))

    # 跳过 OpusHead / OpusTags
    audio: list[bytes] = []
    for pkt in packets:
        if pkt.startswith(b"OpusHead") or pkt.startswith(b"OpusTags"):
            continue
        audio.append(pkt)

    if not audio:
        raise OpusToolError("Ogg 里没找到音频包（只有 OpusHead/OpusTags）")

    return [OpusFrame(data=p) for p in audio]


# ─────────────────────────────────────────────────────────────
# 解码验证：Opus 帧 → WAV
# ─────────────────────────────────────────────────────────────


def opus_frames_to_wav(frames: list[OpusFrame], out_wav: str,
                       sample_rate: int = 24000,
                       frame_duration_ms: int = 60) -> str:
    """把裸 Opus 帧重新封装成 Ogg 再让 ffmpeg 解码成 WAV。

    用于验证：服务器下行发来的音频帧，拼起来能不能解出正常的 PCM。
    如果协议实现有错（漏帧、顺序错、包结构错），解出来的会是噪音或时长不对。

    Args:
        frames: 按时间顺序的 Opus 帧
        out_wav: 输出 WAV 路径
        sample_rate: 下行用 24000
        frame_duration_ms: 60

    Returns:
        out_wav
    """
    if not frames:
        raise OpusToolError("没有音频帧可解码")

    with tempfile.TemporaryDirectory() as tmp:
        ogg_path = os.path.join(tmp, "recovered.ogg")
        write_ogg_opus(ogg_path, frames, sample_rate, frame_duration_ms)
        cmd = [
            _ffmpeg(), "-hide_banner", "-loglevel", "error", "-y",
            "-i", ogg_path,
            "-ar", str(sample_rate),
            "-ac", "1",
            "-c:a", "pcm_s16le",
            out_wav,
        ]
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            raise OpusToolError(
                f"ffmpeg 解码失败（exit {proc.returncode}）:\n{proc.stderr.strip()}"
            )
    return out_wav


def _build_ogg_crc_table() -> list[int]:
    """Ogg 使用的 CRC32（多项式 0x04c11db7，非反射，初值 0，无最终异或）。

    注意这与 zlib 的 CRC32 完全不同（zlib 是反射 + 初值/终值取反）。
    ffmpeg 会校验每页的 CRC，填 0 会报 "CRC mismatch!"。
    """
    poly = 0x04C11DB7
    table = []
    for i in range(256):
        r = i << 24
        for _ in range(8):
            if r & 0x80000000:
                r = ((r << 1) ^ poly) & 0xFFFFFFFF
            else:
                r = (r << 1) & 0xFFFFFFFF
        table.append(r)
    return table


_OGG_CRC_TABLE = _build_ogg_crc_table()


def _ogg_crc(data: bytes) -> int:
    crc = 0
    for b in data:
        crc = ((crc << 8) & 0xFFFFFFFF) ^ _OGG_CRC_TABLE[((crc >> 24) & 0xFF) ^ b]
    return crc


def write_ogg_opus(path: str, frames: list[OpusFrame],
                   sample_rate: int = 24000,
                   frame_duration_ms: int = 60) -> None:
    """把裸 Opus 帧封装成 Ogg Opus 文件。

    需要构造三个东西：
    1. OpusHead（标识页）
    2. OpusTags（注释页，可以为空）
    3. 音频页（每帧一页，简化 segment table 处理）

    每页都计算正确的 Ogg CRC32，否则 ffmpeg 会拒绝解析。
    """
    FIXED = 48000  # Opus 内部固定 48kHz
    samples_per_frame = FIXED * frame_duration_ms // 1000
    serial = 0x1234_5678

    def build_page(header_type: int, granule: int, seq: int,
                   payload: bytes) -> bytes:
        # segment table：每 255 字节一个 255 段，末尾不足 255 的用一个段
        segs = []
        remaining = len(payload)
        while remaining >= 255:
            segs.append(255)
            remaining -= 255
        segs.append(remaining)
        if len(segs) > 255:
            raise OpusToolError("单页 segment 过多（帧太大）")

        header = bytearray()
        header += b"OggS"
        header += bytes([0, header_type])
        header += struct.pack("<q", granule)
        header += struct.pack("<I", serial)
        header += struct.pack("<I", seq)
        header += struct.pack("<I", 0)  # checksum 占位，下面回填
        header += bytes([len(segs)])
        header += bytes(segs)

        page = bytes(header) + payload
        checksum = _ogg_crc(page)
        # checksum 位于页头第 22..26 字节
        patched = bytearray(page)
        patched[22:26] = struct.pack("<I", checksum)
        return bytes(patched)

    with open(path, "wb") as f:
        # ① OpusHead
        head = bytearray()
        head += b"OpusHead"
        head += bytes([1])                       # version
        head += bytes([1])                       # channel count
        head += struct.pack("<H", 312)           # pre-skip
        head += struct.pack("<I", FIXED)         # input sample rate
        head += struct.pack("<h", 0)             # output gain
        head += bytes([0])                       # mapping family
        f.write(build_page(0x02, 0, 0, bytes(head)))

        # ② OpusTags
        tags = bytearray()
        tags += b"OpusTags"
        vendor = b"haimen-sim"
        tags += struct.pack("<I", len(vendor)) + vendor
        tags += struct.pack("<I", 0)             # user comment count
        f.write(build_page(0x00, 0, 1, bytes(tags)))

        # ③ 音频页
        granule = samples_per_frame
        for i, fr in enumerate(frames):
            is_last = i == len(frames) - 1
            # 最后一页标记 EOS（0x04）
            header_type = 0x04 if is_last else 0x00
            f.write(build_page(header_type, granule, 2 + i, fr.data))
            granule += samples_per_frame


# ─────────────────────────────────────────────────────────────
# 生成测试音频
# ─────────────────────────────────────────────────────────────


def make_test_wav(out_wav: str, text_duration_s: float = 2.0,
                  sample_rate: int = 16000) -> str:
    """生成一段测试 WAV（正弦扫频，能让 ASR 收到"有声音"但不保证识别出文字）。

    仅供协议连通性测试：验证 UDP 音频链路能把包送到 ASR、ASR 会返回结果。
    要验证真实语音识别，请用真实录音的 WAV 替换。

    Args:
        out_wav: 输出路径
        text_duration_s: 时长
        sample_rate: 采样率

    Returns:
        out_wav
    """
    with tempfile.TemporaryDirectory() as tmp:
        raw_pcm = os.path.join(tmp, "pcm.raw")
        n_samples = int(sample_rate * text_duration_s)
        # 440Hz 正弦，振幅 0.3 满幅
        import math
        with open(raw_pcm, "wb") as f:
            for i in range(n_samples):
                v = int(math.sin(2 * math.pi * 440.0 * i / sample_rate) * 9800)
                f.write(struct.pack("<h", v))
        cmd = [
            _ffmpeg(), "-hide_banner", "-loglevel", "error", "-y",
            "-f", "s16le", "-ar", str(sample_rate), "-ac", "1",
            "-i", raw_pcm,
            out_wav,
        ]
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            raise OpusToolError(f"生成 WAV 失败:\n{proc.stderr.strip()}")
    return out_wav


if __name__ == "__main__":
    import sys

    if len(sys.argv) < 2:
        print("用法: opus_codec.py <wav 路径>  # 打印切出的 Opus 帧数与大小")
        print("      opus_codec.py --make-test <输出.wav>")
        sys.exit(1)

    if sys.argv[1] == "--make-test":
        out = sys.argv[2] if len(sys.argv) > 2 else "test.wav"
        make_test_wav(out)
        print(f"已生成测试 WAV: {out}")
        sys.exit(0)

    frames = wav_to_opus_frames(sys.argv[1])
    total_ms = len(frames) * 60
    print(f"帧数: {len(frames)}  总时长: {total_ms}ms")
    sizes = [f.size for f in frames]
    print(f"包大小: min={min(sizes)} max={max(sizes)} avg={sum(sizes)//len(sizes)}")
