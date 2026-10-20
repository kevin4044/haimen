/** TTS 提供商元信息 */

export interface ProviderField {
  /** 字段键名（存到 providers[providerId][key]） */
  key: string;
  /** 界面显示标签 */
  label: string;
  /** 输入类型 */
  type: 'password' | 'text' | 'number' | 'select';
  /** 空值时的占位提示 */
  placeholder?: string;
  /** select 类型的选项列表 */
  options?: string[];
  /** number 类型的最小值 */
  min?: number;
  /** number 类型的最大值 */
  max?: number;
  /** number 类型的步长 */
  step?: number;
}

export interface ProviderInfo {
  /** 唯一标识（如 "doubao" "qwen"） */
  id: string;
  /** 显示名称 */
  name: string;
  /** 配置字段列表 */
  fields: ProviderField[];
}

/** 所有支持的 TTS 服务商 */
export const TTS_PROVIDERS: ProviderInfo[] = [
  {
    id: 'doubao',
    name: '火山引擎',
    fields: [
      {
        key: 'api_key',
        label: 'API Key',
        type: 'password',
        placeholder: '未设置，可用环境变量 DOUBAO_API_KEY',
      },
      {
        key: 'resource_id',
        label: '资源 ID',
        type: 'select',
        placeholder: 'seed-tts-2.0',
        options: ['seed-tts-2.0', 'seed-tts-1.0'],
      },
      {
        key: 'cluster',
        label: 'Cluster',
        type: 'select',
        placeholder: '未设置（默认使用 2.0 资源）',
        options: ['volcano_icl'],
      },
    ],
  },
  {
    id: 'qwen',
    name: '阿里通义千问',
    fields: [
      { key: 'api_key', label: 'API Key', type: 'password' },
      {
        key: 'model',
        label: '模型',
        type: 'select',
        placeholder: 'qwen3-tts-instruct-flash-realtime',
        options: ['qwen3-tts-instruct-flash-realtime', 'qwen3-tts-flash-realtime'],
      },
    ],
  },
  {
    id: 'glm',
    name: '智谱AI',
    fields: [
      { key: 'api_key', label: 'API Key', type: 'password' },
      { key: 'model', label: '模型', type: 'text', placeholder: 'glm-tts' },
    ],
  },
  {
    id: 'minimax',
    name: 'MiniMax',
    fields: [
      { key: 'api_key', label: 'API Key', type: 'password' },
      {
        key: 'model',
        label: '模型',
        type: 'select',
        placeholder: 'speech-2.8-hd',
        options: [
          'speech-2.8-hd',
          'speech-2.6-hd',
          'speech-2.6-turbo',
          'speech-2.5-turbo',
          'speech-02-hd',
          'speech-02',
          'speech-01-hd',
          'speech-01',
        ],
      },
      {
        key: 'speed',
        label: '语速',
        type: 'number',
        placeholder: '1.0',
        min: 0.5,
        max: 2,
        step: 0.1,
      },
    ],
  },
  {
    id: 'openai',
    name: 'OpenAI',
    fields: [
      {
        key: 'api_key',
        label: 'API Key',
        type: 'password',
        placeholder: '未设置，可用环境变量 OPENAI_API_KEY',
      },
      {
        key: 'base_url',
        label: 'Base URL',
        type: 'text',
        placeholder: 'https://api.openai.com/v1',
      },
      { key: 'model', label: '模型', type: 'text', placeholder: 'gpt-4o-mini-tts' },
      { key: 'voice', label: '音色', type: 'text', placeholder: 'alloy' },
    ],
  },
];

/** 按 id 查找提供商信息 */
export function getProviderInfo(id: string): ProviderInfo | undefined {
  return TTS_PROVIDERS.find((p) => p.id === id);
}
