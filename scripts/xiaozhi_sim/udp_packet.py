#!/usr/bin/env python3
"""小智 UDP 音频包编解码（模拟器用）

这是 haimen 侧 `crates/haimen-xiaozhi/src/protocol_udp.rs` 的 Python 镜像实现，
用于模拟器与 haimen 互通验证。

协议要点（全部来自固件源码核验）：
    偏移  长度  字段
    0     1    type          服务器 nonce 模板 [0]，固定 0x01
    1     1    flags         服务器 nonce 模板 [1]，固定 0x00
    2     2    payload_len   大端，覆写
    4     4    ssrc          服务器 nonce 模板 [4..8)，设备保留原样
    8     4    timestamp     大端，覆写（非毫秒语义，见下）
    12    4    sequence      大端，覆写，首包 >= 1 严格递增
    16    N    payload       AES-128-CTR 加密的 Opus

★ 关键：这 16 字节包头 **同时就是 AES-CTR 的初始计数器块**。
  加密时：以服务器下发的 nonce 为模板 → 覆盖 len/timestamp/seq → 用整 16 字节当 IV
  解密时：**必须**用收到的包自己的前 16 字节当 IV，不能用固定的 nonce

★ timestamp 语义（Agent A 源码核验）：
  默认 CONFIG_USE_SERVER_AEC=n 时它是不透明字段，设备上行恒为 0。
  但它参与 IV 构造 —— 上下行共用同一对 key，若上下行包头完全相同就会
  keystream 重用。所以：
    · 上行（模拟设备）：timestamp = 0（与真设备一致）
    · 下行（haimen）：timestamp 必须非 0 → 上下行 IV 永不碰撞
  本文件对下行填入以 1 起始的毫秒值。
"""

from __future__ import annotations

import struct
from dataclasses import dataclass

from aes_ctr import aes128_ctr

HEADER_SIZE = 16

# 单包最大总长（组件 recv 缓冲 1500）
MAX_DATAGRAM = 1500


class UdpPacketError(ValueError):
    """UDP 包编解码错误"""


@dataclass
class UdpHeader:
    packet_type: int
    flags: int
    payload_len: int
    ssrc: int
    timestamp: int
    sequence: int

    def encode(self) -> bytes:
        if not 0 <= self.payload_len <= 0xFFFF:
            raise UdpPacketError(f"payload_len 超出 u16 范围: {self.payload_len}")
        return struct.pack(
            ">BBHIII",
            self.packet_type,
            self.flags,
            self.payload_len,
            self.ssrc,
            self.timestamp,
            self.sequence,
        )

    @classmethod
    def decode(cls, buf: bytes) -> "UdpHeader":
        if len(buf) < HEADER_SIZE:
            raise UdpPacketError(f"包头至少要 {HEADER_SIZE} 字节，实际 {len(buf)}")
        t, f, ln, ssrc, ts, seq = struct.unpack(">BBHIII", buf[:HEADER_SIZE])
        return cls(t, f, ln, ssrc, ts, seq)


def parse_hex16(s: str, field: str) -> bytes:
    """解析 32 位 hex 字符串为 16 字节"""
    s = s.strip()
    if len(s) != 32:
        raise UdpPacketError(f"{field} 必须是 32 个 hex 字符（16 字节），实际 {len(s)}")
    try:
        b = bytes.fromhex(s)
    except ValueError as exc:
        raise UdpPacketError(f"{field} 含非 hex 字符: {exc}") from exc
    if len(b) != 16:
        raise UdpPacketError(f"{field} 解码后长度不是 16: {len(b)}")
    return b


class UdpCrypto:
    """一个会话的 UDP 加解密上下文

    `base_nonce` 是服务器在 hello 里下发的 16 字节 nonce 模板：
        [0]    = 0x01  (type)
        [1]    = 0x00  (flags)
        [2..4) 待覆写 (payload_len)
        [4..8) = ssrc（同时用作会话标识）
        [8..12) 待覆写 (timestamp)
        [12..16) 待覆写 (sequence)
    """

    def __init__(self, key: bytes, base_nonce: bytes):
        if len(key) != 16:
            raise UdpPacketError(f"key 必须 16 字节，实际 {len(key)}")
        if len(base_nonce) != 16:
            raise UdpPacketError(f"nonce 必须 16 字节，实际 {len(base_nonce)}")
        self.key = key
        self.base_nonce = base_nonce

    @property
    def session_id(self) -> bytes:
        """会话 ID = nonce[4..8)（上下行都会保留这 4 字节）"""
        return self.base_nonce[4:8]

    @property
    def ssrc(self) -> int:
        return struct.unpack(">I", self.base_nonce[4:8])[0]

    def build_header(self, payload_len: int, timestamp: int, sequence: int) -> UdpHeader:
        """以 base_nonce 为模板构造成出站包头"""
        return UdpHeader(
            packet_type=self.base_nonce[0],
            flags=self.base_nonce[1],
            payload_len=payload_len,
            ssrc=self.ssrc,
            timestamp=timestamp,
            sequence=sequence,
        )

    def seal(self, header: UdpHeader, payload: bytes) -> bytes:
        """加密并组包：返回 16 字节头 + 密文载荷"""
        if len(payload) != header.payload_len:
            raise UdpPacketError(
                f"payload 长度 {len(payload)} 与 header.payload_len {header.payload_len} 不符"
            )
        raw_header = header.encode()
        body = aes128_ctr(self.key, raw_header, payload)
        packet = raw_header + body
        if len(packet) > MAX_DATAGRAM:
            raise UdpPacketError(
                f"单包 {len(packet)} 字节超过上限 {MAX_DATAGRAM}（设备 recv 缓冲限制）"
            )
        return packet

    def open(self, packet: bytes) -> tuple[UdpHeader, bytes]:
        """解包：用包自身前 16 字节当 IV，解密载荷"""
        if len(packet) < HEADER_SIZE:
            raise UdpPacketError(f"包至少 {HEADER_SIZE} 字节，实际 {len(packet)}")
        header = UdpHeader.decode(packet)
        body = packet[HEADER_SIZE:]
        if len(body) != header.payload_len:
            raise UdpPacketError(
                f"声明长度 {header.payload_len} 与实际载荷 {len(body)} 不符"
            )
        # ★ 用线上的原始 16 字节当 IV，而不是重新编码 header
        iv = packet[:HEADER_SIZE]
        plain = aes128_ctr(self.key, iv, body)
        return header, plain


def make_server_nonce(session_id: int) -> bytes:
    """构造服务器下发的 nonce 模板

    布局：
        [0..2)   = 01 00        （type / flags，设备会原样保留）
        [2..4)   = 0000         占位，设备覆盖为 payload_len
        [4..8)   = session_id   会话 ID **同时就是 ssrc**（设备原样保留）
        [8..12)  = 00000000     占位，设备覆盖为 timestamp
        [12..16) = 00000000     占位，设备覆盖为 sequence

    注意 `session_id` 与协议的 `ssrc` 是同一组 4 字节 —— 设备保留哪 4 字节，
    服务器就用哪 4 字节识别会话。不能拆成两个参数。
    """
    if not 0 <= session_id <= 0xFFFFFFFF:
        raise UdpPacketError(f"session_id 超出 u32 范围: {session_id}")
    return bytes([0x01, 0x00, 0x00, 0x00]) + struct.pack(">I", session_id) + bytes(8)


def validate_server_nonce(nonce: bytes) -> None:
    """校验服务器下发的 nonce 是否符合模板约定

    type 必须是 0x01 —— 否则设备发出的包 type 也是错的（设备保留 nonce[0..2)）。
    """
    if len(nonce) != 16:
        raise UdpPacketError(f"nonce 必须 16 字节，实际 {len(nonce)}")
    if nonce[0] != 0x01:
        raise UdpPacketError(
            f"nonce[0] 必须是 0x01（type），实际 0x{nonce[0]:02x}"
            " —— 设备会原样保留它，type 错会导致设备丢包"
        )


if __name__ == "__main__":
    import sys

    print("== UDP 包编解码自检 ==")
    ok = True

    # 构造一个会话
    key = bytes(range(16))
    nonce = make_server_nonce(0x11223344)
    validate_server_nonce(nonce)
    crypto = UdpCrypto(key, nonce)
    print(f"  session_id/ssrc = 0x{crypto.ssrc:08x}")

    # 下行方向：timestamp 非 0（规避 IV 复用）
    h = crypto.build_header(180, timestamp=60, sequence=1)
    assert h.packet_type == 0x01, h.packet_type
    assert h.ssrc == 0x11223344, hex(h.ssrc)
    payload = bytes(range(180))
    pkt = crypto.seal(h, payload)
    print(f"  组包: {len(pkt)} 字节（16 头 + 180 载荷）")

    # 字节序检查
    assert pkt[0] == 0x01 and pkt[1] == 0x00
    assert pkt[2:4] == b"\x00\xb4", pkt[2:4].hex()  # 180 = 0x00B4
    assert pkt[8:12] == b"\x00\x00\x00\x3c", pkt[8:12].hex()  # 60
    assert pkt[12:16] == b"\x00\x00\x00\x01", pkt[12:16].hex()  # 1
    print("  ✅ 各字段大端字节序正确")

    # 解包
    h2, plain = crypto.open(pkt)
    assert plain == payload, "解密结果不一致"
    assert (h2.timestamp, h2.sequence, h2.payload_len) == (60, 1, 180)
    print("  ✅ seal/open round-trip 通过")

    # IV 差异检查：上行 ts=0 与下行 ts=60 的包头必须不同
    up_h = crypto.build_header(180, timestamp=0, sequence=1)
    if up_h.encode() == h.encode():
        print("  ❌ 上行/下行包头相同 → 存在 IV 复用风险")
        ok = False
    else:
        print("  ✅ 上行(ts=0) 与下行(ts=60) 包头不同，无 IV 复用")

    # 长度不符要报错
    try:
        crypto.open(pkt[:100])
        print("  ❌ 截断包未报错")
        ok = False
    except UdpPacketError:
        print("  ✅ 截断包正确报错")

    # 非法 hex
    try:
        parse_hex16("abc", "key")
        print("  ❌ 非法 hex 未报错")
        ok = False
    except UdpPacketError:
        print("  ✅ 非法 hex 正确报错")

    # nonce 模板校验
    try:
        validate_server_nonce(bytes(16))
        print("  ❌ 错误 nonce 未报错")
        ok = False
    except UdpPacketError:
        print("  ✅ nonce[0] != 0x01 正确报错")

    sys.exit(0 if ok else 1)
