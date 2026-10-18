#!/usr/bin/env python3
"""AES-128-CTR 实现（模拟器用）

优先级：
1. 若 `cryptography` 可用 → 用它的 C 实现（快）
2. 否则 → 用本文件的纯 Python 实现（慢但零依赖、保证可用）

为什么需要这个：
模拟器要能独立完成「与 haimen 的 UDP 音频包加解密」，
而小智设备侧只关心字节，不关心实现。测试环境里 `cryptography`
的原生扩展可能因 macOS 代码签名问题加载失败（实测遇到过），
所以必须有一个纯 Python 的兜底。

纯 Python 的性能对模拟器完全够用：下行 24kHz/60ms ≈ 180 字节/包，
每秒 16.7 包 → 约 3 KB/s。纯 Python AES 处理 3 KB/s 毫无压力。

正确性验证：内置 NIST SP 800-38A 的 CTR-AES128 测试向量。
"""

from __future__ import annotations

# ─────────────────────────────────────────────────────────────
# 尝试使用 cryptography（快）
# ─────────────────────────────────────────────────────────────

_BACKEND = None
try:  # pragma: no cover - 取决于具体环境
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

    def _ctr_crypt_cryptography(key: bytes, iv: bytes, data: bytes) -> bytes:
        enc = Cipher(algorithms.AES(key), modes.CTR(iv)).encryptor()
        return enc.update(data) + enc.finalize()

    # 用 NIST 向量做一次自检，通过才认为可用
    _K = bytes.fromhex("2b7e151628aed2a6abf7158809cf4f3c")
    _IV = bytes.fromhex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff")
    _PT = bytes.fromhex("6bc1bee22e409f96e93d7e117393172a")
    _EXPECT = "874d6191b620e3261bef6864990db6ce"
    if _ctr_crypt_cryptography(_K, _IV, _PT).hex() == _EXPECT:
        _BACKEND = "cryptography"
except Exception:  # 导入或自检失败都退回纯 Python
    _BACKEND = None


# ─────────────────────────────────────────────────────────────
# 纯 Python AES-128
# ─────────────────────────────────────────────────────────────

_SBOX = [
    0x63, 0x7C, 0x77, 0x7B, 0xF2, 0x6B, 0x6F, 0xC5, 0x30, 0x01, 0x67, 0x2B, 0xFE, 0xD7, 0xAB, 0x76,
    0xCA, 0x82, 0xC9, 0x7D, 0xFA, 0x59, 0x47, 0xF0, 0xAD, 0xD4, 0xA2, 0xAF, 0x9C, 0xA4, 0x72, 0xC0,
    0xB7, 0xFD, 0x93, 0x26, 0x36, 0x3F, 0xF7, 0xCC, 0x34, 0xA5, 0xE5, 0xF1, 0x71, 0xD8, 0x31, 0x15,
    0x04, 0xC7, 0x23, 0xC3, 0x18, 0x96, 0x05, 0x9A, 0x07, 0x12, 0x80, 0xE2, 0xEB, 0x27, 0xB2, 0x75,
    0x09, 0x83, 0x2C, 0x1A, 0x1B, 0x6E, 0x5A, 0xA0, 0x52, 0x3B, 0xD6, 0xB3, 0x29, 0xE3, 0x2F, 0x84,
    0x53, 0xD1, 0x00, 0xED, 0x20, 0xFC, 0xB1, 0x5B, 0x6A, 0xCB, 0xBE, 0x39, 0x4A, 0x4C, 0x58, 0xCF,
    0xD0, 0xEF, 0xAA, 0xFB, 0x43, 0x4D, 0x33, 0x85, 0x45, 0xF9, 0x02, 0x7F, 0x50, 0x3C, 0x9F, 0xA8,
    0x51, 0xA3, 0x40, 0x8F, 0x92, 0x9D, 0x38, 0xF5, 0xBC, 0xB6, 0xDA, 0x21, 0x10, 0xFF, 0xF3, 0xD2,
    0xCD, 0x0C, 0x13, 0xEC, 0x5F, 0x97, 0x44, 0x17, 0xC4, 0xA7, 0x7E, 0x3D, 0x64, 0x5D, 0x19, 0x73,
    0x60, 0x81, 0x4F, 0xDC, 0x22, 0x2A, 0x90, 0x88, 0x46, 0xEE, 0xB8, 0x14, 0xDE, 0x5E, 0x0B, 0xDB,
    0xE0, 0x32, 0x3A, 0x0A, 0x49, 0x06, 0x24, 0x5C, 0xC2, 0xD3, 0xAC, 0x62, 0x91, 0x95, 0xE4, 0x79,
    0xE7, 0xC8, 0x37, 0x6D, 0x8D, 0xD5, 0x4E, 0xA9, 0x6C, 0x56, 0xF4, 0xEA, 0x65, 0x7A, 0xAE, 0x08,
    0xBA, 0x78, 0x25, 0x2E, 0x1C, 0xA6, 0xB4, 0xC6, 0xE8, 0xDD, 0x74, 0x1F, 0x4B, 0xBD, 0x8B, 0x8A,
    0x70, 0x3E, 0xB5, 0x66, 0x48, 0x03, 0xF6, 0x0E, 0x61, 0x35, 0x57, 0xB9, 0x86, 0xC1, 0x1D, 0x9E,
    0xE1, 0xF8, 0x98, 0x11, 0x69, 0xD9, 0x8E, 0x94, 0x9B, 0x1E, 0x87, 0xE9, 0xCE, 0x55, 0x28, 0xDF,
    0x8C, 0xA1, 0x89, 0x0D, 0xBF, 0xE6, 0x42, 0x68, 0x41, 0x99, 0x2D, 0x0F, 0xB0, 0x54, 0xBB, 0x16,
]

_RCON = [
    0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1B, 0x36,
]


def _xtime(a: int) -> int:
    a <<= 1
    if a & 0x100:
        a = (a ^ 0x1B) & 0xFF
    return a & 0xFF


def _expand_key(key: bytes) -> list[list[int]]:
    """AES-128 key expansion：16 字节 key → 11 个 16 字节轮密钥"""
    if len(key) != 16:
        raise ValueError("AES-128 需要 16 字节 key")
    w = [list(key[i * 4:i * 4 + 4]) for i in range(4)]
    for i in range(4, 44):
        temp = list(w[i - 1])
        if i % 4 == 0:
            temp = temp[1:] + temp[:1]
            temp = [_SBOX[b] for b in temp]
            temp[0] ^= _RCON[i // 4 - 1]
        w.append([w[i - 4][j] ^ temp[j] for j in range(4)])
    return [sum(w[r * 4:r * 4 + 4], []) for r in range(11)]


def _encrypt_block(round_keys: list[list[int]], block: bytes) -> bytes:
    """AES-128 单块加密（CTR 只需要加密方向）"""
    state = [block[i] ^ round_keys[0][i] for i in range(16)]

    for rnd in range(1, 11):
        # SubBytes
        state = [_SBOX[b] for b in state]
        # ShiftRows（state 按列主序：state[c*4 + r]）
        state = [
            state[0], state[5], state[10], state[15],
            state[4], state[9], state[14], state[3],
            state[8], state[13], state[2], state[7],
            state[12], state[1], state[6], state[11],
        ]
        # MixColumns（最后一轮不做）
        if rnd != 10:
            new = [0] * 16
            for c in range(4):
                a0, a1, a2, a3 = state[c * 4:c * 4 + 4]
                new[c * 4 + 0] = _xtime(a0) ^ (_xtime(a1) ^ a1) ^ a2 ^ a3
                new[c * 4 + 1] = a0 ^ _xtime(a1) ^ (_xtime(a2) ^ a2) ^ a3
                new[c * 4 + 2] = a0 ^ a1 ^ _xtime(a2) ^ (_xtime(a3) ^ a3)
                new[c * 4 + 3] = (_xtime(a0) ^ a0) ^ a1 ^ a2 ^ _xtime(a3)
            state = new
        # AddRoundKey
        state = [state[i] ^ round_keys[rnd][i] for i in range(16)]

    return bytes(state)


def _ctr_crypt_pure(key: bytes, iv: bytes, data: bytes) -> bytes:
    """CTR 模式：对每个 16 字节块，用 iv + block_index 加密后 XOR"""
    if len(iv) != 16:
        raise ValueError("AES-CTR 需要 16 字节 IV")
    round_keys = _expand_key(key)
    # 把 IV 当 128 位大端整数，逐块 +1
    counter = int.from_bytes(iv, "big")
    out = bytearray(len(data))
    for offset in range(0, len(data), 16):
        ks = _encrypt_block(round_keys, counter.to_bytes(16, "big"))
        chunk = data[offset:offset + 16]
        for i, b in enumerate(chunk):
            out[offset + i] = b ^ ks[i]
        counter = (counter + 1) & ((1 << 128) - 1)
    return bytes(out)


# ─────────────────────────────────────────────────────────────
# 对外统一接口
# ─────────────────────────────────────────────────────────────


def aes128_ctr(key: bytes, iv: bytes, data: bytes) -> bytes:
    """AES-128-CTR 加/解密（CTR 是同一个操作）

    Args:
        key: 16 字节密钥
        iv:  16 字节初始计数器块（小智协议里就是 UDP 包头的 16 字节）
        data: 待处理数据

    Returns:
        长度与 data 完全相同的结果
    """
    if len(key) != 16:
        raise ValueError(f"AES-128 需要 16 字节 key，实际 {len(key)}")
    if len(iv) != 16:
        raise ValueError(f"AES-CTR 需要 16 字节 IV，实际 {len(iv)}")
    if not data:
        return b""
    if _BACKEND == "cryptography":
        return _ctr_crypt_cryptography(key, iv, data)
    return _ctr_crypt_pure(key, iv, data)


def backend() -> str:
    """当前实际使用的后端：'cryptography' 或 'pure-python'"""
    return _BACKEND or "pure-python"


# ─────────────────────────────────────────────────────────────
# 自检
# ─────────────────────────────────────────────────────────────

if __name__ == "__main__":
    import sys

    ok = True

    # NIST SP 800-38A F.5.1 CTR-AES128.Encrypt
    K = bytes.fromhex("2b7e151628aed2a6abf7158809cf4f3c")
    IV = bytes.fromhex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff")
    PT = bytes.fromhex(
        "6bc1bee22e409f96e93d7e117393172a"
        "ae2d8a571e03ac9c9eb76fac45af8e51"
        "30c81c46a35ce411e5fbc1191a0a52ef"
        "f69f2445df4f9b17ad2b417be66c3710"
    )
    EXPECT = (
        "874d6191b620e3261bef6864990db6ce"
        "9806f66b7970fdff8617187bb9fffdff"
        "5ae4df3edbd5d35e5b4f09020db03eab"
        "1e031dda2fbe03d1792170a0f3009cee"
    )

    # 两种后端都验一遍
    for name, fn in (
        ("pure-python", _ctr_crypt_pure),
        ("统一接口", lambda k, i, d: aes128_ctr(k, i, d)),
    ):
        got = fn(K, IV, PT).hex()
        if got == EXPECT:
            print(f"  ✅ {name}: NIST CTR-AES128 向量通过")
        else:
            print(f"  ❌ {name}: 向量不一致\n     期望 {EXPECT}\n     实际 {got}")
            ok = False

    # round-trip + 长度不变
    data = bytes(range(200))
    ct = aes128_ctr(K, IV, data)
    pt = aes128_ctr(K, IV, ct)
    if pt == data and len(ct) == len(data):
        print(f"  ✅ round-trip 通过，长度不变 ({len(data)} → {len(ct)})")
    else:
        print("  ❌ round-trip 失败")
        ok = False

    print(f"  当前后端: {backend()}")
    sys.exit(0 if ok else 1)
