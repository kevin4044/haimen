// 小智传输层配置。类型定义在 haimen-xiaozhi，OTA 与根配置共用同一套结构。
// 本文件由 src/config/settings.rs include，避免再声明一个同名模块。
pub use haimen_xiaozhi::{XiaozhiConfig, XiaozhiMqttConfig, XiaozhiProtocol, XiaozhiUdpConfig};
