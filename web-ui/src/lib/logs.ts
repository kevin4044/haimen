/** 日志级别（与 tracing 文件日志中的级别标记一致） */
export type LogLevel = 'TRACE' | 'DEBUG' | 'INFO' | 'WARN' | 'ERROR';

/** 级别筛选：ALL 表示不按级别过滤 */
export type LogLevelFilter = LogLevel | 'ALL';

/** 级别下拉框的展示顺序 */
export const LOG_LEVELS: readonly LogLevel[] = ['TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR'];

/** 匹配日志行中的级别标记：大小写不敏感，且要求整词匹配 */
export const LEVEL_RE = /\b(TRACE|DEBUG|INFO|WARN|ERROR)\b/i;

/** 从原始日志行中提取级别；无法识别时返回 null */
export function extractLevel(line: string): LogLevel | null {
  const match = LEVEL_RE.exec(line);
  if (!match) return null;
  return match[1].toUpperCase() as LogLevel;
}

export interface LogFilter {
  level: LogLevelFilter;
  keyword: string;
}

/**
 * 按级别与关键字过滤日志行，保持原始顺序。
 * 关键字为大小写不敏感的子串匹配；首尾空白会被忽略，空字符串不过滤。
 */
export function filterLogLines(lines: readonly string[], { level, keyword }: LogFilter): string[] {
  const needle = keyword.trim().toLowerCase();
  return lines.filter((line) => {
    if (level !== 'ALL' && extractLevel(line) !== level) return false;
    if (needle && !line.toLowerCase().includes(needle)) return false;
    return true;
  });
}

/**
 * 追加日志行并只保留最后 max 行，保持顺序；max <= 0 时返回空数组。
 * 不修改入参数组。
 */
export function appendCapped(
  existing: readonly string[],
  incoming: readonly string[],
  max: number,
): string[] {
  const cap = Math.max(0, Math.floor(max));
  if (cap === 0) return [];
  const merged = [...existing, ...incoming];
  return merged.length <= cap ? merged : merged.slice(merged.length - cap);
}

/**
 * 暂停期间累积待显示行：新行追加到缓冲区，缓冲区同样只保留最后 max 行。
 * 恢复时用 `appendCapped(lines, buffer, max)` 合并即可。
 */
export function accumulatePaused(
  pending: readonly string[],
  incoming: readonly string[],
  max: number,
): string[] {
  return appendCapped(pending, incoming, max);
}
