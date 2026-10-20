/** ASR 提供商元信息 */

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
}

export interface ProviderInfo {
  /** 唯一标识（如 "doubao" "qwen"） */
  id: string;
  /** 显示名称 */
  name: string;
  /** 配置字段列表 */
  fields: ProviderField[];
}

/** 所有支持的 ASR 服务商 */
export const ASR_PROVIDERS: ProviderInfo[] = [
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
        placeholder: 'volc.seedasr.sauc.duration',
        options: ['volc.seedasr.sauc.duration', 'volc.seedasr.sauc.concurrent'],
      },
      {
        key: 'end_window_size',
        label: '判停窗口（毫秒）',
        type: 'number',
        placeholder: '500',
      },
      {
        key: 'force_to_speech_time',
        label: '最小语音时长（毫秒）',
        type: 'number',
        placeholder: '500',
      },
    ],
  },
  {
    id: 'qwen',
    name: '阿里通义千问',
    fields: [{ key: 'api_key', label: 'API Key', type: 'password' }],
  },
  {
    id: 'glm',
    name: '智谱AI',
    fields: [{ key: 'api_key', label: 'API Key', type: 'password' }],
  },
  {
    id: 'mimo',
    name: '小米 MiMo',
    fields: [{ key: 'api_key', label: 'API Key', type: 'password' }],
  },
  {
    id: 'xfyun',
    name: '科大讯飞',
    fields: [
      { key: 'app_id', label: 'App ID', type: 'text' },
      { key: 'api_key', label: 'API Key', type: 'password' },
      { key: 'api_secret', label: 'API Secret', type: 'password' },
    ],
  },
];

/** 按 id 查找提供商信息 */
export function getProviderInfo(id: string): ProviderInfo | undefined {
  return ASR_PROVIDERS.find((p) => p.id === id);
}
