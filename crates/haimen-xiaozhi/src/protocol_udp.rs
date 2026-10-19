//! UDP 音频包的纯函数编解码与 AES-128-CTR。
//!
//! 包头 16 字节明文，其后是加密后的 Opus。计数器块必须是**该包自己的包头**，
//! 而不是 hello 下发的固定 nonce：服务器 nonce 只提供 type/flags/ssrc 模板，
//! 每个包还要覆盖 payload_len、timestamp、sequence。解密时同样用入站包的前 16 字节。

use aes::Aes128;
use ctr::Ctr128BE;
use ctr::cipher::{KeyIvInit, StreamCipher};

/// UDP 音频包头（16 字节，全部明文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpAudioHeader {
    /// nonce[0]，通常 0x01。
    pub packet_type: u8,
    /// nonce[1]，通常 0x00。
    pub flags: u8,
    /// 载荷字节数，线上为大端。
    pub payload_len: u16,
    /// nonce[4..8)。
    pub ssrc: u32,
    /// 时间戳，单位毫秒，线上为大端。
    pub timestamp: u32,
    /// 序列号，线上为大端，从 1 递增。
    pub sequence: u32,
}

impl UdpAudioHeader {
    /// 包头固定长度。
    pub const SIZE: usize = 16;

    /// 序列化为 16 字节大端表头。
    pub fn encode(&self) -> [u8; 16] {
        let mut buf = [0u8; Self::SIZE];
        buf[0] = self.packet_type;
        buf[1] = self.flags;
        buf[2..4].copy_from_slice(&self.payload_len.to_be_bytes());
        buf[4..8].copy_from_slice(&self.ssrc.to_be_bytes());
        buf[8..12].copy_from_slice(&self.timestamp.to_be_bytes());
        buf[12..16].copy_from_slice(&self.sequence.to_be_bytes());
        buf
    }

    /// 从至少 16 字节的切片解析包头。
    pub fn decode(buf: &[u8]) -> Result<Self, UdpProtoError> {
        if buf.len() < Self::SIZE {
            return Err(UdpProtoError::PacketTooShort { len: buf.len() });
        }
        Ok(Self {
            packet_type: buf[0],
            flags: buf[1],
            payload_len: u16::from_be_bytes([buf[2], buf[3]]),
            ssrc: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            timestamp: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
            sequence: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
        })
    }
}

/// 通道密钥（来自服务器 hello 的 `udp.key` / `udp.nonce`）。
#[derive(Debug)]
pub struct UdpCrypto {
    key: [u8; 16],
    /// 基础 nonce：其中 `[0..2)` 是 type/flags，`[4..8)` 是 ssrc。
    /// `[2..4)` 与 `[8..16)` 会在每个包上被长度、时间戳、序列号覆盖。
    base_nonce: [u8; 16],
}

impl UdpCrypto {
    /// 从 hex 字符串构造。key 与 nonce 都必须正好 32 个 hex 字符（16 字节）。
    pub fn from_hex(key_hex: &str, nonce_hex: &str) -> Result<Self, UdpProtoError> {
        Ok(Self {
            key: decode_hex_16("key", key_hex)?,
            base_nonce: decode_hex_16("nonce", nonce_hex)?,
        })
    }

    /// 以 `base_nonce` 为模板构造出站包头，只覆盖长度、时间戳和序列号。
    ///
    /// type/flags/ssrc 必须留在服务器下发的 nonce 里，设备靠这三字段识别通道。
    pub fn build_header(&self, payload_len: u16, timestamp: u32, sequence: u32) -> UdpAudioHeader {
        UdpAudioHeader {
            packet_type: self.base_nonce[0],
            flags: self.base_nonce[1],
            payload_len,
            ssrc: u32::from_be_bytes([
                self.base_nonce[4],
                self.base_nonce[5],
                self.base_nonce[6],
                self.base_nonce[7],
            ]),
            timestamp,
            sequence,
        }
    }

    /// 加密一个出站包，返回 16 字节明文头 + 加密载荷。
    ///
    /// 计数器块是编码后的包头，不是固定的 `base_nonce`。同一密钥下，不同的
    /// 长度、时间戳或序列号会产生不同的密钥流。
    pub fn seal(&self, header: &UdpAudioHeader, payload: &[u8]) -> Result<Vec<u8>, UdpProtoError> {
        if payload.len() > u16::MAX as usize {
            return Err(UdpProtoError::PayloadTooLarge { len: payload.len() });
        }
        if payload.len() != header.payload_len as usize {
            return Err(UdpProtoError::LengthMismatch {
                declared: header.payload_len,
                actual: payload.len(),
            });
        }

        let counter = header.encode();
        let mut body = payload.to_vec();
        aes_ctr_apply(&self.key, &counter, &mut body)?;

        let mut packet = Vec::with_capacity(UdpAudioHeader::SIZE + body.len());
        packet.extend_from_slice(&counter);
        packet.extend_from_slice(&body);
        Ok(packet)
    }

    /// 解密一个入站包。计数器块取该包自己的前 16 字节。
    ///
    /// 不能改用 hello 里的固定 nonce：对方已经按本包的长度、时间戳、序列号
    /// 覆盖过计数器，固定 nonce 解出来是乱码。
    pub fn open(&self, packet: &[u8]) -> Result<(UdpAudioHeader, Vec<u8>), UdpProtoError> {
        if packet.len() < UdpAudioHeader::SIZE {
            return Err(UdpProtoError::PacketTooShort { len: packet.len() });
        }
        let header = UdpAudioHeader::decode(packet)?;
        let payload = &packet[UdpAudioHeader::SIZE..];
        if payload.len() != header.payload_len as usize {
            return Err(UdpProtoError::LengthMismatch {
                declared: header.payload_len,
                actual: payload.len(),
            });
        }

        // 用线上的原始 16 字节当计数器，避免重新组包时和发送端不一致。
        let mut counter = [0u8; UdpAudioHeader::SIZE];
        counter.copy_from_slice(&packet[..UdpAudioHeader::SIZE]);

        let mut plain = payload.to_vec();
        aes_ctr_apply(&self.key, &counter, &mut plain)?;
        Ok((header, plain))
    }
}

/// UDP 音频包编解码错误。
///
/// `haimen-xiaozhi` 没有依赖 `thiserror`，本任务也不允许改依赖清单，
/// 所以这里手写 `Display` + `Error`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdpProtoError {
    /// hex 字符串非法（长度不是 32，或含非 hex 字符）。
    InvalidHex {
        /// 字段名：`"key"` 或 `"nonce"`。
        field: &'static str,
        /// 失败原因。
        reason: String,
    },
    /// 数据包短于 16 字节头。
    PacketTooShort {
        /// 实际字节数。
        len: usize,
    },
    /// `payload_len` 字段与实际载荷长度不符。
    LengthMismatch {
        /// 包头声明的长度。
        declared: u16,
        /// 实际载荷字节数。
        actual: usize,
    },
    /// 载荷过长，无法用 `u16` 表示。
    PayloadTooLarge {
        /// 实际字节数。
        len: usize,
    },
    /// AES-CTR 初始化失败。密钥与计数器都是 16 字节时理论上不会发生。
    CryptoInit {
        /// 底层错误描述。
        reason: String,
    },
}

impl std::fmt::Display for UdpProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidHex { field, reason } => {
                write!(f, "UDP 字段 {field} 的十六进制非法：{reason}")
            }
            Self::PacketTooShort { len } => {
                write!(f, "UDP 音频包长度 {len} 短于 16 字节包头")
            }
            Self::LengthMismatch { declared, actual } => {
                write!(
                    f,
                    "UDP 载荷长度不符：声明 {declared} 字节，实际 {actual} 字节"
                )
            }
            Self::PayloadTooLarge { len } => {
                write!(f, "UDP 载荷长度 {len} 超过 u16 上限")
            }
            Self::CryptoInit { reason } => {
                write!(f, "AES-CTR 初始化失败：{reason}")
            }
        }
    }
}

impl std::error::Error for UdpProtoError {}

fn decode_hex_16(field: &'static str, hex: &str) -> Result<[u8; 16], UdpProtoError> {
    if hex.len() != 32 {
        return Err(UdpProtoError::InvalidHex {
            field,
            reason: format!("长度必须为 32 个十六进制字符，实际 {}", hex.len()),
        });
    }
    let bytes = hex.as_bytes();
    let mut out = [0u8; 16];
    for i in 0..16 {
        let hi = hex_nibble(bytes[i * 2]).ok_or_else(|| UdpProtoError::InvalidHex {
            field,
            reason: format!("第 {} 个字符不是十六进制", i * 2),
        })?;
        let lo = hex_nibble(bytes[i * 2 + 1]).ok_or_else(|| UdpProtoError::InvalidHex {
            field,
            reason: format!("第 {} 个字符不是十六进制", i * 2 + 1),
        })?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 用 16 字节计数器块对 `buf` 就地做 AES-128-CTR。
///
/// `ctr` 的 `Ctr128BE` 把整个 16 字节当作大端计数器，从该块开始加密后再加一。
/// 这与固件 `mbedtls_aes_crypt_ctr` 的递增方式一致，所以计数器必须等于包头。
fn aes_ctr_apply(key: &[u8; 16], counter: &[u8; 16], buf: &mut [u8]) -> Result<(), UdpProtoError> {
    let mut cipher = Ctr128BE::<Aes128>::new_from_slices(key, counter).map_err(|err| {
        UdpProtoError::CryptoInit {
            reason: err.to_string(),
        }
    })?;
    cipher.apply_keystream(buf);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "00112233445566778899aabbccddeeff";
    /// type/flags = 01 A0，被覆盖的槽填成明显的垃圾值，ssrc = 11223344。
    const NONCE_HEX: &str = "01A0AABB11223344DEADBEEFCAFEBABE";

    fn crypto() -> UdpCrypto {
        UdpCrypto::from_hex(KEY_HEX, NONCE_HEX).unwrap()
    }

    #[test]
    fn header_round_trip() {
        let header = UdpAudioHeader {
            packet_type: 0x01,
            flags: 0xA0,
            payload_len: 0xBEEF,
            ssrc: 0x1122_3344,
            timestamp: 0x1234_5678,
            sequence: 0x9ABC_DEF0,
        };
        assert_eq!(UdpAudioHeader::decode(&header.encode()).unwrap(), header);
    }

    #[test]
    fn timestamp_is_big_endian() {
        let header = UdpAudioHeader {
            packet_type: 0x01,
            flags: 0x00,
            payload_len: 0,
            ssrc: 0,
            timestamp: 0x1234_5678,
            sequence: 1,
        };
        let buf = header.encode();
        assert_eq!(&buf[8..12], &[0x12, 0x34, 0x56, 0x78]);
    }

    #[test]
    fn payload_len_is_big_endian() {
        let header = UdpAudioHeader {
            packet_type: 0x01,
            flags: 0x00,
            payload_len: 300,
            ssrc: 0,
            timestamp: 0,
            sequence: 1,
        };
        let buf = header.encode();
        assert_eq!(&buf[2..4], &[0x01, 0x2C]);
    }

    #[test]
    fn seal_open_round_trip() {
        let plain = b"opus-frame-payload";
        let header = crypto().build_header(plain.len() as u16, 60, 1);
        let packet = crypto().seal(&header, plain).unwrap();
        let (opened, decrypted) = crypto().open(&packet).unwrap();
        assert_eq!(opened, header);
        assert_eq!(decrypted, plain);
    }

    #[test]
    fn counter_block_matches_known_ciphertext() {
        // 包头即计数器。向量由 OpenSSL AES-128-CTR（与 NIST SP 800-38A 的
        // 大端计数器一致）对这 20 字节明文算出，覆盖两个 AES 块。
        let plain = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff, 0x00, 0x11, 0x22, 0x33,
        ];
        let header = crypto().build_header(plain.len() as u16, 0x3c, 1);
        let packet = crypto().seal(&header, &plain).unwrap();

        let counter = header.encode();
        assert_eq!(&packet[..16], &counter);
        assert_eq!(
            counter,
            [
                0x01, 0xA0, 0x00, 0x14, 0x11, 0x22, 0x33, 0x44, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x00,
                0x00, 0x01,
            ]
        );
        assert_eq!(
            &packet[16..],
            [
                0xc0, 0xcd, 0x5a, 0xd0, 0x6c, 0x57, 0xc0, 0x90, 0x1c, 0x95, 0xf5, 0xbc, 0x02, 0x5d,
                0x6d, 0x3f, 0x8e, 0x59, 0x91, 0x6b,
            ]
        );

        let (opened, decrypted) = crypto().open(&packet).unwrap();
        assert_eq!(opened.encode(), counter);
        assert_eq!(decrypted, plain);
    }

    #[test]
    fn length_mismatch_is_rejected() {
        let header = crypto().build_header(4, 60, 1);
        let err = crypto().seal(&header, b"ab").unwrap_err();
        assert_eq!(
            err,
            UdpProtoError::LengthMismatch {
                declared: 4,
                actual: 2,
            }
        );

        let packet = crypto().seal(&header, b"abcd").unwrap();
        let mut truncated = packet.clone();
        truncated.pop();
        assert_eq!(
            crypto().open(&truncated).unwrap_err(),
            UdpProtoError::LengthMismatch {
                declared: 4,
                actual: 3,
            }
        );

        let mut extra = packet;
        extra.push(0);
        assert_eq!(
            crypto().open(&extra).unwrap_err(),
            UdpProtoError::LengthMismatch {
                declared: 4,
                actual: 5,
            }
        );
    }

    #[test]
    fn packet_shorter_than_header_is_rejected() {
        assert_eq!(
            UdpAudioHeader::decode(&[0u8; 15]).unwrap_err(),
            UdpProtoError::PacketTooShort { len: 15 }
        );
        assert_eq!(
            crypto().open(&[]).unwrap_err(),
            UdpProtoError::PacketTooShort { len: 0 }
        );
        assert_eq!(
            crypto().open(&[0u8; 15]).unwrap_err(),
            UdpProtoError::PacketTooShort { len: 15 }
        );
    }

    #[test]
    fn invalid_hex_character_is_rejected() {
        let bad_key = "00112233445566778899aabbccddeefg";
        let err = UdpCrypto::from_hex(bad_key, NONCE_HEX).unwrap_err();
        assert!(matches!(
            err,
            UdpProtoError::InvalidHex { field: "key", .. }
        ));
    }

    #[test]
    fn hex_length_must_be_32() {
        let err = UdpCrypto::from_hex("0011", NONCE_HEX).unwrap_err();
        assert_eq!(
            err,
            UdpProtoError::InvalidHex {
                field: "key",
                reason: "长度必须为 32 个十六进制字符，实际 4".to_string(),
            }
        );
        let long_nonce = format!("{}cd", "ab".repeat(16));
        let err = UdpCrypto::from_hex(KEY_HEX, &long_nonce).unwrap_err();
        assert!(matches!(
            err,
            UdpProtoError::InvalidHex { field: "nonce", .. }
        ));
    }

    #[test]
    fn empty_payload_round_trip() {
        let header = crypto().build_header(0, 0, 1);
        let packet = crypto().seal(&header, b"").unwrap();
        assert_eq!(packet.len(), 16);
        let (opened, plain) = crypto().open(&packet).unwrap();
        assert_eq!(opened, header);
        assert!(plain.is_empty());
    }

    #[test]
    fn build_header_keeps_type_flags_and_ssrc() {
        let header = crypto().build_header(300, 0x1234_5678, 7);
        let encoded = header.encode();
        let nonce = decode_hex_16("nonce", NONCE_HEX).unwrap();

        assert_eq!(&encoded[..2], &nonce[..2]);
        assert_eq!(&encoded[4..8], &nonce[4..8]);
        assert_ne!(&encoded[2..4], &nonce[2..4]);
        assert_ne!(&encoded[8..], &nonce[8..]);
        assert_eq!(header.packet_type, 0x01);
        assert_eq!(header.flags, 0xA0);
        assert_eq!(header.ssrc, 0x1122_3344);
        assert_eq!(header.payload_len, 300);
        assert_eq!(header.timestamp, 0x1234_5678);
        assert_eq!(header.sequence, 7);
    }

    #[test]
    fn payload_larger_than_u16_is_rejected() {
        let header = crypto().build_header(0, 0, 1);
        let payload = vec![0u8; u16::MAX as usize + 1];
        assert_eq!(
            crypto().seal(&header, &payload).unwrap_err(),
            UdpProtoError::PayloadTooLarge {
                len: u16::MAX as usize + 1,
            }
        );
    }
}
