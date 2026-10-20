//! 设备目录：把设备身份（MAC / MQTT client_id）映射到「名字 + 房间」。
//!
//! # 为什么需要
//!
//! MQTT + UDP 传输层本身已经按 `client_id` 隔离会话，多台设备可以同时在线；
//! 但传输层只知道 MAC，不知道「这台设备在哪个房间」。把房间信息做成一层
//! 静态目录以后：
//!
//! 1. 对话链路可以在把用户文本交给 LLM 之前，注入房间上下文；
//! 2. notify 接口可以按「设备名」或「房间」寻址，而不是只能填 MAC。
//!
//! # 配置
//!
//! ```toml
//! [[xiaozhi.devices]]
//! device_id = "84:c7:bb:59:93:64"  # MAC（冒号/下划线/连字符皆可）或 MQTT client_id
//! name = "客厅小智"
//! room = "客厅"
//! ```
//!
//! 设备目录是进程级全局槽（和 notify 的设备门面一样），由启动流程安装一次。
//! 没配置时目录为空：对话不注入房间上下文，notify 仍可按 MAC/client_id 寻址。

use std::sync::{Arc, RwLock};

use crate::types::XiaozhiDeviceConfig;

/// 一台已配置设备。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredDevice {
    /// 配置里写的原始身份（MAC 或 client_id），回显日志/响应时用。
    pub device_id: String,
    /// 归一化后的冒号 MAC。client_id 会先取出 `@@@` 之间的 MAC 段。
    pub mac: String,
    /// 便于人识别的名字；未配置时回退为归一化 MAC。
    pub name: String,
    /// 所在房间；空串 = 未分配。
    pub room: String,
}

impl ConfiguredDevice {
    /// 展示名：配置了用配置的，否则用 MAC。
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.mac
        } else {
            &self.name
        }
    }
}

/// 设备身份归一化：把 MAC 的各种写法统一成冒号小写形式。
///
/// - `84:c7:bb:59:93:64` → 原样
/// - `84_c7_bb_59_93_64` → 冒号形式
/// - `84c7bb599364`      → 冒号形式
///
/// 不是 MAC 的输入（如 `unknown`）原样返回小写，调用方仍可用它做精确匹配。
pub fn normalize_mac(raw: &str) -> String {
    let mac = raw.trim().to_ascii_lowercase();
    if mac.contains(':') {
        return mac;
    }
    if mac.contains('_') {
        return mac.replace('_', ":");
    }
    if mac.len() == 12 && mac.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let mut out = String::with_capacity(17);
        for (idx, chunk) in mac.as_bytes().chunks(2).enumerate() {
            if idx > 0 {
                out.push(':');
            }
            out.push_str(&String::from_utf8_lossy(chunk));
        }
        return out;
    }
    mac
}

/// 从 MQTT `client_id` 提取设备 MAC。
///
/// 小智固件的 client_id 形如 `GID_<型号>@@@<mac>@@@<mac>`；
/// 不含 `@@@` 时（例如用户在测试里直接写 MAC 当 client_id）原样归一化。
pub fn device_id_from_client_id(client_id: &str) -> String {
    let mut parts = client_id.split("@@@").filter(|part| !part.is_empty());
    let _group = parts.next();
    let mac = match parts.next() {
        Some(mac) => mac,
        None => client_id,
    };
    normalize_mac(mac)
}

/// 配置里的设备身份归一化：client_id 先取出 MAC 段，MAC 写法统一成冒号形式。
pub fn normalize_device_id(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains("@@@") {
        device_id_from_client_id(raw)
    } else {
        normalize_mac(raw)
    }
}

/// 已配置设备目录。查找全部按大小写不敏感处理。
#[derive(Debug, Clone, Default)]
pub struct DeviceDirectory {
    devices: Vec<ConfiguredDevice>,
}

impl DeviceDirectory {
    /// 从 `[[xiaozhi.devices]]` 配置构建。空身份/重复 MAC 的条目会被跳过。
    pub fn from_config(configs: &[XiaozhiDeviceConfig]) -> Self {
        let mut devices = Vec::with_capacity(configs.len());
        for cfg in configs {
            let raw_id = cfg.device_id.trim();
            if raw_id.is_empty() {
                continue;
            }
            let mac = normalize_device_id(raw_id);
            if devices.iter().any(|d: &ConfiguredDevice| d.mac == mac) {
                tracing::warn!(
                    device_id = raw_id,
                    mac = %mac,
                    "设备目录里出现重复设备，保留先出现的一条"
                );
                continue;
            }
            let name = cfg.name.trim().to_string();
            devices.push(ConfiguredDevice {
                device_id: raw_id.to_string(),
                mac,
                name,
                room: cfg.room.trim().to_string(),
            });
        }
        Self { devices }
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// 按配置顺序返回所有已配置设备。
    pub fn entries(&self) -> &[ConfiguredDevice] {
        &self.devices
    }

    /// 按任意身份查找：client_id（精确）、MAC（归一化后比较）或设备名。
    pub fn find(&self, identity: &str) -> Option<&ConfiguredDevice> {
        let want = identity.trim();
        if want.is_empty() {
            return None;
        }
        let want_mac = normalize_device_id(want);
        self.devices.iter().find(|device| {
            device.device_id.eq_ignore_ascii_case(want)
                || device.mac.eq_ignore_ascii_case(&want_mac)
                || device.name.trim().eq_ignore_ascii_case(want)
        })
    }

    /// 按 MQTT client_id 查找（client_id 里含 MAC 也能匹配）。
    pub fn for_client_id(&self, client_id: &str) -> Option<&ConfiguredDevice> {
        let mac = device_id_from_client_id(client_id);
        self.devices.iter().find(|device| {
            device.device_id.eq_ignore_ascii_case(client_id.trim())
                || device.mac.eq_ignore_ascii_case(&mac)
        })
    }

    /// 某房间里的所有已配置设备。
    pub fn in_room(&self, room: &str) -> Vec<&ConfiguredDevice> {
        let room = room.trim();
        if room.is_empty() {
            return Vec::new();
        }
        self.devices
            .iter()
            .filter(|device| device.room.eq_ignore_ascii_case(room))
            .collect()
    }

    /// 构造注入给 LLM 的房间上下文前缀。未配置房间时返回 `None`。
    pub fn context_for(&self, device: &ConfiguredDevice) -> Option<String> {
        let room = device.room.trim();
        if room.is_empty() {
            return None;
        }
        Some(format!(
            "【设备上下文】当前设备「{}」位于「{}」，用户就在这个房间和你说话；回答时可以结合这个房间的语境。\n",
            device.display_name(),
            room
        ))
    }

    /// 按 MQTT client_id 构造房间上下文前缀。
    pub fn context_for_client(&self, client_id: &str) -> Option<String> {
        self.context_for(self.for_client_id(client_id)?)
    }
}

static DEVICE_DIRECTORY: RwLock<Option<Arc<DeviceDirectory>>> = RwLock::new(None);

/// 安装设备目录（覆盖式）。
///
/// 用 `RwLock<Option<...>>` 而不是 `OnceLock`：测试会反复安装不同目录，
/// 覆盖式安装让用例之间互不污染；生产路径启动时只装一次。
pub fn install_device_directory(directory: Arc<DeviceDirectory>) {
    let mut slot = DEVICE_DIRECTORY
        .write()
        .unwrap_or_else(|err| err.into_inner());
    *slot = Some(directory);
}

/// 当前设备目录。未安装时返回 `None`（等同于没有任何设备配置）。
pub fn device_directory() -> Option<Arc<DeviceDirectory>> {
    DEVICE_DIRECTORY
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

/// 快捷安装：直接从配置切片构建并安装。
pub fn install_devices_from_config(configs: &[XiaozhiDeviceConfig]) {
    install_device_directory(Arc::new(DeviceDirectory::from_config(configs)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(device_id: &str, name: &str, room: &str) -> XiaozhiDeviceConfig {
        XiaozhiDeviceConfig {
            device_id: device_id.to_string(),
            name: name.to_string(),
            room: room.to_string(),
        }
    }

    #[test]
    fn normalize_mac_accepts_common_spellings() {
        assert_eq!(normalize_mac("84:C7:BB:59:93:64"), "84:c7:bb:59:93:64");
        assert_eq!(normalize_mac("84_c7_bb_59_93_64"), "84:c7:bb:59:93:64");
        assert_eq!(normalize_mac("84c7bb599364"), "84:c7:bb:59:93:64");
        assert_eq!(normalize_mac(" unknown "), "unknown");
    }

    #[test]
    fn device_id_from_client_id_extracts_mac() {
        assert_eq!(
            device_id_from_client_id("GID_haimen@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64"),
            "84:c7:bb:59:93:64"
        );
        assert_eq!(
            device_id_from_client_id("84:c7:bb:59:93:64"),
            "84:c7:bb:59:93:64"
        );
    }

    #[test]
    fn directory_resolves_by_mac_client_id_and_name() {
        let dir = DeviceDirectory::from_config(&[
            cfg("84:c7:bb:59:93:64", "客厅小智", "客厅"),
            cfg(
                "GID_haimen@@@aa_bb_cc_dd_ee_ff@@@aa_bb_cc_dd_ee_ff",
                "",
                "卧室",
            ),
        ]);

        let living = dir.find("84_C7_BB_59_93_64").expect("按 MAC 查找");
        assert_eq!(living.room, "客厅");
        assert_eq!(living.display_name(), "客厅小智");

        let by_name = dir.find("客厅小智").expect("按设备名查找");
        assert_eq!(by_name.mac, "84:c7:bb:59:93:64");

        let bedroom = dir
            .for_client_id("GID_haimen@@@aa_bb_cc_dd_ee_ff@@@aa_bb_cc_dd_ee_ff")
            .expect("按 client_id 查找");
        assert_eq!(bedroom.mac, "aa:bb:cc:dd:ee:ff");
        // 没配名字时展示名回退为 MAC
        assert_eq!(bedroom.display_name(), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn directory_handles_rooms_and_context() {
        let dir = DeviceDirectory::from_config(&[
            cfg("84:c7:bb:59:93:64", "客厅小智", "客厅"),
            cfg("aa:bb:cc:dd:ee:ff", "卧室小智", "卧室"),
            cfg("11:22:33:44:55:66", "书房小智", ""),
        ]);

        assert_eq!(dir.in_room("客厅").len(), 1);
        assert_eq!(dir.in_room(" 卧室 ").len(), 1);
        assert!(dir.in_room("厨房").is_empty());

        let context = dir
            .context_for_client("GID_haimen@@@84_c7_bb_59_93_64@@@84_c7_bb_59_93_64")
            .expect("应生成房间上下文");
        assert!(context.contains("客厅"), "{context}");
        assert!(context.contains("客厅小智"), "{context}");

        // 未分配房间 → 不注入，避免给 LLM 无意义噪音。
        assert!(
            dir.context_for_client("11:22:33:44:55:66").is_none(),
            "未配置房间的设备不应注入上下文"
        );
        // 未配置的设备 → 不注入。
        assert!(dir.context_for_client("ff:ff:ff:ff:ff:ff").is_none());
    }

    #[test]
    fn duplicate_mac_keeps_first_entry() {
        let dir = DeviceDirectory::from_config(&[
            cfg("84:c7:bb:59:93:64", "第一个", "客厅"),
            cfg("84_C7_BB_59_93_64", "第二个", "卧室"),
        ]);
        assert_eq!(dir.entries().len(), 1);
        assert_eq!(dir.entries()[0].room, "客厅");
    }
}
