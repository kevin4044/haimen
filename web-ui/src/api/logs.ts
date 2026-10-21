import type { RuntimeLogBatch, RuntimeLogMeta } from '@/types';

/** 后端默认回看的日志行数 */
export const DEFAULT_LOG_TAIL = 200;

export interface OpenLogStreamOptions {
  /** 初次连接回看的行数，默认 200 */
  tail?: number;
  onMeta: (meta: RuntimeLogMeta) => void;
  onLines: (batch: RuntimeLogBatch) => void;
  /** 连接出错时回调；EventSource 会自动重连，不会主动关闭 */
  onError: (event: Event) => void;
  onOpen?: () => void;
}

/** 安全解析 SSE 事件的 JSON data；格式非法时返回 null */
function parseEventData(event: Event): unknown {
  const data = (event as MessageEvent<unknown>).data;
  if (typeof data !== 'string' || data.length === 0) return null;
  try {
    return JSON.parse(data) as unknown;
  } catch {
    return null;
  }
}

function isRuntimeLogMeta(value: unknown): value is RuntimeLogMeta {
  if (typeof value !== 'object' || value === null) return false;
  const meta = value as Record<string, unknown>;
  return typeof meta.path === 'string' && typeof meta.tail === 'number';
}

function isRuntimeLogBatch(value: unknown): value is RuntimeLogBatch {
  if (typeof value !== 'object' || value === null) return false;
  const lines = (value as Record<string, unknown>).lines;
  return Array.isArray(lines) && lines.every((line) => typeof line === 'string');
}

/**
 * 打开运行日志 SSE 流并注册 meta / log 事件监听。
 *
 * 返回关闭函数；调用方负责在卸载时关闭。连接出错时仅通过 onError 通知，
 * 由 EventSource 自身按浏览器策略自动重连。
 *
 * 当前环境不支持 EventSource 时抛出错误，便于页面展示降级提示。
 */
export function openLogStream({
  tail = DEFAULT_LOG_TAIL,
  onMeta,
  onLines,
  onError,
  onOpen,
}: OpenLogStreamOptions): () => void {
  if (typeof EventSource === 'undefined') {
    throw new Error('当前环境不支持 EventSource，无法接收实时日志');
  }

  const source = new EventSource(`/api/v1/logs/stream?tail=${encodeURIComponent(String(tail))}`);

  const handleMeta = (event: Event) => {
    const payload = parseEventData(event);
    if (isRuntimeLogMeta(payload)) onMeta(payload);
  };

  const handleLog = (event: Event) => {
    const payload = parseEventData(event);
    if (isRuntimeLogBatch(payload)) onLines(payload);
  };

  const handleOpen = () => {
    onOpen?.();
  };

  const handleError = (event: Event) => {
    onError(event);
  };

  source.addEventListener('meta', handleMeta);
  source.addEventListener('log', handleLog);
  source.addEventListener('open', handleOpen);
  source.addEventListener('error', handleError);

  return () => {
    source.removeEventListener('meta', handleMeta);
    source.removeEventListener('log', handleLog);
    source.removeEventListener('open', handleOpen);
    source.removeEventListener('error', handleError);
    source.close();
  };
}
