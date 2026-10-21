// ── 通用 API 响应 ──

export interface ApiResponse<T = unknown> {
  success: boolean;
  data?: T;
  error?: string;
  message?: string;
  /** 保存成功但需要重启 haimen 才生效 */
  restart_required?: boolean;
}

// ── 系统信息 ──

export interface SystemInfo {
  version: string;
  uptime: number;
  connectors: ConnectorStatus[];
  agents: AgentStatus[];
}

export interface ConnectorStatus {
  name: string;
  type: string;
  connected: boolean;
  error?: string;
}

export interface AgentStatus {
  name: string;
  type: string;
  available: boolean;
}

// ── 配置 ──

export interface GatewayConfig {
  connectors: ConnectorConfig[];
  agents: AgentConfig[];
  web: WebConfig;
}

export interface ConnectorConfig {
  name: string;
  type: 'lark' | 'github' | 'telegram';
  enabled: boolean;
  [key: string]: unknown;
}

export interface AgentConfig {
  name: string;
  type: 'claude_code' | 'mcp';
  enabled: boolean;
  [key: string]: unknown;
}

export interface WebConfig {
  host: string;
  port: number;
}

// ── 会话 ──

export interface Session {
  id: string;
  connector: string;
  created_at: string;
  updated_at: string;
  message_count: number;
}

// ── 日志 ──

export interface LogEntry {
  timestamp: string;
  level: 'info' | 'warn' | 'error' | 'debug';
  message: string;
  source?: string;
}

// ── 语音配置 ──

export interface AsrSettings {
  /** 当前激活的服务商 */
  active_provider: string;
  /** 所有已配置提供商的凭证 { provider_name: { key: value } } */
  providers: Record<string, Record<string, string>>;
  /** 当前激活提供商的回退解析值 */
  resolved?: {
    api_key: string | null;
  };
}

export interface TtsSettings {
  /** 当前激活的服务商 */
  active_provider: string;
  /** 所有已配置提供商的凭证 { provider_name: { key: value } } */
  providers: Record<string, Record<string, string>>;
  /** 是否启用固定文本模式 */
  fixed_text_enabled: boolean;
  /** 固定文本内容 */
  fixed_text: string | null;
  /** 是否在设备唤醒时主动播报问候 */
  wake_greeting_enabled: boolean;
  /** 唤醒问候文案（空串回退「你好」） */
  wake_greeting: string | null;
  /** 无语音超时后的告别文案（空串回退「拜拜」） */
  no_speech_goodbye: string | null;
  /** 录音开始后累计无有效语音达到该毫秒数时播报告别并关闭连接（0 禁用） */
  no_speech_timeout_ms: number;
  /** 是否在等待 Agent 首个可播文本期间播报处理进度提示 */
  thinking_feedback_enabled: boolean;
  /** 进度提示播报间隔毫秒（0 = 仅保留超时兜底） */
  thinking_feedback_interval_ms: number;
  /** 周期进度提示文案（空串回退默认） */
  thinking_feedback_text: string | null;
  /** 等待超时的兜底文案（空串回退默认） */
  thinking_feedback_timeout_text: string | null;
  /** 当前激活提供商的回退解析值 */
  resolved?: {
    api_key: string | null;
    voice: string | null;
  };
}

export interface TtsVoice {
  id: string;
  name: string;
  language: string;
  /** 所属模型（豆包为 seed-tts-1.0 / seed-tts-2.0，其他提供商可能缺失） */
  model?: string;
}

// ── Agent 配置 ──

export interface AgentSettings {
  /** 当前激活的 Agent 提供商 */
  active_provider: string;
  /** 所有已配置提供商的参数 { provider_name: { key: value } } */
  providers: Record<string, Record<string, string>>;
  /** 当前激活提供商的回退解析值 */
  resolved?: {
    agent: string;
  };
  /** 保存是否已热切换生效 */
  applied?: boolean;
  /** 运行时实际生效的 Agent 提供商 */
  applied_agent?: string;
  /** 当前 Agent 代数（每次切换 +1） */
  generation?: number;
}

// ── 消息渠道（Connector）配置 ──

/** 单个渠道的可用状态（L1 配置层 + L2 认证层） */
export type ConnectorState = 'online' | 'auth_failed' | 'misconfigured' | 'disabled';

export interface ConnectorStatusItem {
  id: string;
  name: string;
  configured: boolean;
  enabled: boolean;
  auth_ok: boolean;
  status: ConnectorState;
  detail: string;
}

export interface LarkSettings {
  enabled: boolean;
}

export interface DingTalkSettings {
  enabled: boolean;
  client_id: string;
  client_secret: string;
  allow_from: string;
  share_session_in_channel: boolean;
  robot_code: string;
}

export interface ConnectorSettings {
  lark: LarkSettings;
  dingtalk: DingTalkSettings;
}

// ── Agent 调用日志 ──

export type AgentLogSource = 'gateway' | 'xiaozhi' | 'cli';
export type AgentLogStatus = 'success' | 'error' | 'timeout';

/** 思考事件（模型推理过程） */
export interface AgentLogThinkingEvent {
  type: 'thinking';
  thinking: string;
}

/** 工具调用事件 */
export interface AgentLogToolUseEvent {
  type: 'tool_use';
  id: string;
  name: string;
  /** 最终拼接好的入参 JSON */
  input: unknown;
}

/** 工具执行结果事件 */
export interface AgentLogToolResultEvent {
  type: 'tool_result';
  tool_use_id: string;
  content: string;
  is_error?: boolean;
}

/** 内容轨迹事件（thinking / tool_use / tool_result），按出现顺序 */
export type AgentLogEvent = AgentLogThinkingEvent | AgentLogToolUseEvent | AgentLogToolResultEvent;

/** 一次 Agent 调用的日志记录（对应后端 agent_log::AgentLogRecord） */
export interface AgentLogRecord {
  timestamp: string;
  source: AgentLogSource;
  agent: string;
  connector: string | null;
  chat_id: string | null;
  sender_id: string | null;
  session_id: string | null;
  work_dir: string;
  input: string;
  output: string | null;
  status: AgentLogStatus;
  error: string | null;
  latency_ms: number;
  /** 完整内容轨迹；老记录可能缺省 */
  events?: AgentLogEvent[];
}

export interface AgentLogsData {
  /** 日志功能是否启用（后端 [agent_log] enabled） */
  enabled: boolean;
  records: AgentLogRecord[];
}

// ── 小智设备 ──

/** 一台已配置的小智设备（对应后端 `[[xiaozhi.devices]]`，device_id 已归一化 MAC） */
export interface XiaozhiConfiguredDevice {
  device_id: string;
  name: string;
  room: string;
  configured: boolean;
}

/** 当前在线的小智设备（来自 MQTT/UDP 传输层实时状态） */
export interface XiaozhiOnlineDevice {
  client_id: string;
  device_id: string;
  name: string;
  room: string;
  online: boolean;
  state: XiaozhiDeviceState;
  configured: boolean;
}

/** 设备忙闲状态：idle = 可下发通知，listening/playing = 正在对话 */
export type XiaozhiDeviceState = 'idle' | 'listening' | 'playing';

/** `GET /api/v1/settings/xiaozhi/devices` 的 data */
export interface XiaozhiDevicesData {
  devices: XiaozhiConfiguredDevice[];
  online: XiaozhiOnlineDevice[];
}

// ── 小智传输 / OTA / 主动播报配置 ──

export interface XiaozhiMqttSettings {
  endpoint: string;
  username: string;
  password: string;
  auth_key: string;
  auth_grace: boolean;
  keepalive: number;
  publish_topic_template: string;
  downlink_topic_template: string;
}

export interface XiaozhiUdpSettings {
  bind_addr: string;
}

/** `GET/PUT /api/v1/settings/xiaozhi` 的 data（设备目录在单独接口） */
export interface XiaozhiSettings {
  protocol: 'websocket' | 'mqtt';
  mqtt: XiaozhiMqttSettings;
  udp: XiaozhiUdpSettings;
  ota_allow_private_only: boolean;
  ota_token: string;
  ota_trusted_proxies: string[];
  say_token: string;
}

/** `POST /api/v1/xiaozhi/say` 成功响应 */
export interface XiaozhiSayResult {
  ok: true;
  device: string;
  client_id: string;
  name: string;
  room: string;
  mode: string;
  audio_url: string;
  duration_ms: number;
}

/** `POST /api/v1/xiaozhi/say` / devices 的失败响应 */
export interface XiaozhiErrorBody {
  ok: false;
  error: string;
  detail?: string;
  state?: string;
  room?: string;
  devices?: XiaozhiOnlineDevice[];
}

// ── 运行日志 ──

export interface RuntimeLogMeta {
  path: string;
  tail: number;
}

export interface RuntimeLogBatch {
  lines: string[];
}
