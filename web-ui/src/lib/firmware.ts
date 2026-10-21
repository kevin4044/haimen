import { XiaozhiError } from '@/api/xiaozhi';
import type {
  XiaozhiFirmwareCandidate,
  XiaozhiFirmwareDevice,
  XiaozhiFirmwareRelease,
  XiaozhiFirmwareState,
} from '@/types';

/** 两位补零（本地时间格式化用） */
function pad2(value: number): string {
  return String(value).padStart(2, '0');
}

/** 毫秒时间戳 / ISO 字符串 → 本地 `YYYY-MM-DD HH:mm`；无法解析时返回 null */
function toLocalDateTime(value: number | string): string | null {
  let date: Date;
  if (typeof value === 'number') {
    date = new Date(value);
  } else {
    const raw = value.trim();
    // 后端字段一般是 ISO 字符串，这里也兼容纯毫秒时间戳（10~13 位数字）
    const numeric = /^\d{10,13}$/.test(raw) ? Number(raw) : Number.NaN;
    date = new Date(Number.isNaN(numeric) ? raw : numeric);
  }
  if (Number.isNaN(date.getTime())) return null;
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())} ${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
}

/** release 发布时间只展示日期部分 `YYYY-MM-DD`；缺失/非法返回空串 */
function toDateOnly(publishedAt?: string | null): string {
  const raw = publishedAt?.trim();
  if (!raw) return '';
  const date = new Date(raw);
  if (Number.isNaN(date.getTime())) return '';
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}`;
}

/**
 * 版本号展示：去掉首尾空白并统一小写 `v` 前缀；空值回退「未知版本」。
 * 例：`2.5.2` → `v2.5.2`，`V2.5.2` → `v2.5.2`，`v2.5.2` → `v2.5.2`。
 */
export function formatVersion(version?: string | null): string {
  const value = version?.trim();
  if (!value) return '未知版本';
  return /^v/i.test(value) ? `v${value.slice(1)}` : `v${value}`;
}

/**
 * 设备上报时间（毫秒时间戳）→ 本地 `YYYY-MM-DD HH:mm`。
 * 缺省、非正数或非法值回退「未上报」。
 */
export function formatReportTime(ms?: number | null): string {
  if (ms == null || !Number.isFinite(ms) || ms <= 0) return '未上报';
  return toLocalDateTime(ms) ?? '未上报';
}

/** 上游版本信息获取时间（ISO 字符串或毫秒时间戳）→ 本地 `YYYY-MM-DD HH:mm` */
export function formatFetchedAt(value?: string | null): string {
  const raw = value?.trim();
  if (!raw) return '未知';
  return toLocalDateTime(raw) ?? raw;
}

/** 固件与上游版本关系的展示元信息（Badge 文案 + 着色） */
export const FIRMWARE_STATE_META: Record<
  XiaozhiFirmwareState,
  { label: string; className: string }
> = {
  update_available: {
    label: '可升级',
    className: 'bg-primary/10 text-primary border-transparent',
  },
  latest: {
    label: '已是最新',
    className: 'bg-green-500/15 text-green-600 border-transparent',
  },
  needs_selection: {
    label: '需选择',
    className: 'bg-orange-500/15 text-orange-600 border-transparent',
  },
  ahead: {
    label: '领先上游',
    className: 'bg-secondary text-secondary-foreground border-transparent',
  },
  unknown: {
    label: '待上报',
    className: 'text-muted-foreground',
  },
  unsupported: {
    label: '不支持',
    className: 'bg-yellow-500/15 text-yellow-600 border-transparent',
  },
  error: {
    label: '异常',
    className: 'bg-destructive/10 text-destructive border-transparent',
  },
};

/** 取固件状态的 Badge 文案与 class；未知状态兜底为「待上报」 */
export function firmwareStateMeta(state: XiaozhiFirmwareState): {
  label: string;
  className: string;
} {
  return (
    FIRMWARE_STATE_META[state] ?? {
      label: '待上报',
      className: 'text-muted-foreground',
    }
  );
}

/**
 * release 的展示文案：`v2.5.2 · 2026-10-11`。
 * `tag` 缺失时回退 `version`，发布日期缺失/非法时只展示版本号。
 */
export function releaseLabel(
  release: Pick<XiaozhiFirmwareRelease, 'tag' | 'version' | 'published_at'>,
): string {
  const tag = formatVersion(release.tag || release.version);
  const date = toDateOnly(release.published_at);
  return date ? `${tag} · ${date}` : tag;
}

/**
 * 升级弹窗的默认版本：优先该设备匹配到的最新 release（`latest.tag`），
 * 否则回退到上游 release 列表的第一项（约定为最新），都没有时返回空串。
 */
export function defaultVersionForDevice(
  device: Pick<XiaozhiFirmwareDevice, 'latest'> | null | undefined,
  releases: readonly XiaozhiFirmwareRelease[],
): string {
  const latestTag = device?.latest?.tag?.trim();
  if (latestTag) return latestTag;
  return releases[0]?.tag ?? '';
}

/**
 * 候选固件的展示文案：`sp-esp32-s3-1.28-box · esp32s3 · 构建名匹配`。
 * 芯片型号不匹配时追加「⚠ 芯片不匹配」（后端会以 `chip_mismatch` 拒绝升级）。
 */
export function candidateLabel(candidate: XiaozhiFirmwareCandidate): string {
  const primary = candidate.name?.trim() || candidate.full_name?.trim() || '未知固件';
  const parts = [primary];
  const target = candidate.target?.trim();
  const reason = candidate.reason?.trim();
  if (target) parts.push(target);
  if (reason) parts.push(reason);
  const label = parts.join(' · ');
  return candidate.chip_match ? label : `${label} · ⚠ 芯片不匹配`;
}

/**
 * 把升级 / 清除目标时抛出的 `XiaozhiError` 映射为可操作的中文提示。
 * 非 `XiaozhiError`（网络错误等）给出通用提示。
 */
export function describeFirmwareError(error: unknown): string {
  if (!(error instanceof XiaozhiError)) {
    return '操作失败：请检查网络连接或 haimen 后端日志。';
  }
  const detail = error.detail ? `（${error.detail}）` : '';
  switch (error.code) {
    case 'invalid_request':
      return `请求参数不合法：请确认设备与版本号后重试${detail}。`;
    case 'device_unknown':
      return `未找到该设备：请先在「小智设备」中登记设备，并确认设备已重启上报${detail}。`;
    case 'release_not_found':
      return `上游没有这个版本：请刷新版本列表后选择已有版本，或检查手动输入的版本号${detail}。`;
    case 'board_not_found':
      return `该版本没有匹配当前板型的固件：请确认设备上报的板型，或换一个版本重试${detail}。`;
    case 'board_ambiguous':
      return `该版本匹配到多个板型固件：请稍后重试，或到上游 release 确认板型${detail}。`;
    case 'selection_required':
      return `未精确匹配板型：请从候选固件中选择要刷入的版本后再确认${detail}。`;
    case 'variant_not_found':
      return `所选固件变体不存在：请重新选择候选固件后再试${detail}。`;
    case 'chip_mismatch':
      return `芯片型号不匹配：该固件不能刷入当前设备，请确认板型 / 芯片型号${detail}。`;
    case 'asset_not_found':
      return `上游 release 缺少固件文件：请换一个版本或稍后重试${detail}。`;
    case 'download_failed':
      return `固件下载失败：请检查 haimen 主机的网络与代理后重试${detail}。`;
    case 'checksum_mismatch':
      return `固件校验失败（SHA-256 不一致）：已放弃本次升级，请重新发起${detail}。`;
    case 'extract_failed':
      return `固件解压失败：压缩包可能损坏，请重试或换一个版本${detail}。`;
    case 'firmware_too_large':
      return `固件超过设备 OTA 分区大小：请选择更小的版本，或检查设备上报的分区大小${detail}。`;
    case 'cache_write_failed':
      return `固件缓存写入失败：请检查 haimen 主机的磁盘空间与目录权限${detail}。`;
    case 'github_unavailable':
      return `GitHub 不可用：请检查网络或代理，稍后点击「刷新版本信息」重试${detail}。`;
    case 'github_rate_limited':
      return `GitHub API 触发限流：请稍后再试（可在 haimen 主机上配置 GitHub Token 提高限额）${detail}。`;
    case 'upgrade_in_progress':
      return `该设备已有升级任务进行中：请等待下载完成或稍后重试${detail}。`;
    case 'firmware_store_unavailable':
      return `固件存储不可用：请检查后端固件目录是否存在且可写${detail}。`;
    default:
      return `操作失败（${error.code}）：${error.message}`;
  }
}
