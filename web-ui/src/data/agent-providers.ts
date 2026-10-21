/** Agent 提供商元信息 */

/** 单条可配置字段（沿用 ASR/TTS 的 ProviderField 本地声明模式） */
export interface ProviderField {
  /** 配置 key，对应 settings.toml 中 providers.<id>.key */
  key: string;
  /** 界面显示名 */
  label: string;
  /** 输入控件类型 */
  type: 'password' | 'text' | 'select';
  placeholder?: string;
  options?: string[];
}

export interface ProviderInfo {
  /** 唯一标识（如 "claude-code" "codex"） */
  id: string;
  /** 显示名称 */
  name: string;
  /** 该提供商的可配置字段（如 cli_path） */
  fields: ProviderField[];
}

/** 所有支持的 Agent 提供商 */
export const AGENT_PROVIDERS: ProviderInfo[] = [
  {
    id: 'claude-code',
    name: 'Claude Code',
    fields: [
      {
        key: 'cli_path',
        label: 'CLI 路径',
        type: 'text',
        placeholder: '留空使用 PATH 查找 claude',
      },
    ],
  },
  {
    id: 'codex',
    name: 'Codex CLI',
    fields: [
      {
        key: 'cli_path',
        label: 'CLI 路径',
        type: 'text',
        placeholder: '留空使用 PATH 查找 codex',
      },
      {
        key: 'sandbox',
        label: '沙箱策略',
        type: 'select',
        placeholder: 'danger-full-access',
        options: ['danger-full-access', 'workspace-write', 'read-only'],
      },
    ],
  },
  {
    id: 'openclaw',
    name: 'OpenClaw',
    fields: [
      {
        key: 'cli_path',
        label: 'CLI 路径',
        type: 'text',
        placeholder: '留空使用 PATH 查找 openclaw',
      },
      {
        key: 'agent',
        label: 'Agent ID',
        type: 'text',
        placeholder: 'main',
      },
    ],
  },
  {
    id: 'hermes',
    name: 'Hermes',
    fields: [
      {
        key: 'cli_path',
        label: 'CLI 路径',
        type: 'text',
        placeholder: '留空使用 PATH 查找 hermes',
      },
    ],
  },
  {
    id: 'openai',
    name: 'OpenAI 兼容',
    fields: [
      {
        key: 'base_url',
        label: 'Base URL',
        type: 'text',
        placeholder: 'http://127.0.0.1:8642/v1',
      },
      {
        key: 'model',
        label: '模型',
        type: 'text',
        placeholder: '例如 qwen2.5:7b',
      },
      {
        key: 'api_key',
        label: 'API Key',
        type: 'password',
        placeholder: '留空则不发送 Authorization',
      },
    ],
  },
];

/** 按 id 查找提供商信息 */
export function getAgentProviderInfo(id: string): ProviderInfo | undefined {
  return AGENT_PROVIDERS.find((p) => p.id === id);
}
