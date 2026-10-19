//! MQTT + UDP 传输层端到端演示服务
//!
//! 用途：在没有真机、也不想动线上配置的情况下，把 MQTT+UDP 传输层单独跑起来，
//! 配合 `scripts/xiaozhi_sim/device_sim.py` 做全链路验证。
//!
//! 它做三件事：
//! 1. 在本机起内置 MQTT broker（默认 0.0.0.0:1883）
//! 2. 起 UDP 音频通道（默认 0.0.0.0:8888）
//! 3. 用 `EchoStrategy` 驱动：把设备上行录到的音频原样回放给设备
//!
//! 用 EchoStrategy 的原因：它不需要 ASR/LLM/TTS 任何外部依赖，
//! 能把「MQTT 控制流 + UDP 音频流 + 帧节奏」这三件事单独验证干净。
//!
//! 运行：
//! ```bash
//! cargo run -p haimen-xiaozhi --example mqtt_udp_demo
//! # 或指定端口
//! BROKER_BIND=0.0.0.0:18830 UDP_BIND=0.0.0.0:18880 \
//!   cargo run -p haimen-xiaozhi --example mqtt_udp_demo
//! ```
//!
//! 然后在另一个终端跑模拟器：
//! ```bash
//! python3 scripts/xiaozhi_sim/device_sim.py --broker 127.0.0.1:1883
//! ```

use std::sync::Arc;

use haimen_xiaozhi::mqtt_broker::MqttBrokerConfig;
use haimen_xiaozhi::udp_audio::UdpAudioConfig;
use haimen_xiaozhi::{EchoStrategy, MqttUdpConfig, MqttUdpTransport, ResponseStrategy};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 允许通过环境变量改端口，方便同时跑多份互不干扰
    let broker_bind = std::env::var("BROKER_BIND").unwrap_or_else(|_| "0.0.0.0:1883".to_owned());
    let udp_bind = std::env::var("UDP_BIND").unwrap_or_else(|_| "0.0.0.0:8888".to_owned());

    let cfg = MqttUdpConfig {
        broker: MqttBrokerConfig {
            bind_addr: broker_bind.clone(),
            max_packet_size: 256 * 1024,
            auth_key: None,
            legacy_password: None,
        },
        udp: UdpAudioConfig {
            bind_addr: udp_bind.clone(),
        },
        // 设备只认 OTA 下发的 publish_topic；这里与 OTA 默认模板保持一致
        publish_topic_template: "haimen/xz/{mac}/up".to_owned(),
        downlink_topic_template: "haimen/xz/{mac}/down".to_owned(),
        prebuffer_count: 5,
        frame_duration_ms: 60,
        // 演示服务跑在同一网段，回退到自动探测即可
        advertise_ip: std::env::var("ADVERTISE_IP").ok(),
    };

    let strategy: Arc<dyn ResponseStrategy> = Arc::new(EchoStrategy);

    println!("=== MQTT + UDP 传输层演示 ===");
    println!("  MQTT broker: {broker_bind}");
    println!("  UDP 音频:    {udp_bind}");
    println!("  策略:        echo（把上行音频原样回放）");
    println!();

    let transport = MqttUdpTransport::start(cfg, strategy).await?;

    println!("已启动，等待设备连接…");
    println!("  实际 broker: {}", transport.broker_addr());
    println!("  实际 UDP:    {}", transport.udp_addr());
    println!();
    println!("用模拟器测试：");
    println!(
        "  python3 scripts/xiaozhi_sim/device_sim.py --broker 127.0.0.1:{}",
        transport.broker_addr().rsplit(':').next().unwrap_or("1883")
    );
    println!();
    println!("Ctrl-C 退出");

    // 每 5 秒打印一次在线设备数，便于观察连接状态
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n收到 Ctrl-C，正在关闭…");
                break;
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                let n = transport.online_device_count().await;
                if n > 0 {
                    println!("在线设备: {n}");
                }
            }
        }
    }

    transport.shutdown().await;
    println!("已关闭");
    Ok(())
}
