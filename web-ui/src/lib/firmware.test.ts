import { describe, expect, it } from 'vitest';
import { XiaozhiError } from '@/api/xiaozhi';
import type { XiaozhiFirmwareCandidate, XiaozhiFirmwareRelease } from '@/types';
import {
  candidateLabel,
  defaultVersionForDevice,
  describeFirmwareError,
  FIRMWARE_STATE_META,
  firmwareStateMeta,
  formatFetchedAt,
  formatReportTime,
  formatVersion,
  releaseLabel,
} from './firmware';

function makeRelease(overrides: Partial<XiaozhiFirmwareRelease> = {}): XiaozhiFirmwareRelease {
  return {
    tag: 'v2.5.2',
    version: '2.5.2',
    name: 'v2.5.2',
    published_at: '2026-10-11T12:00:00Z',
    prerelease: false,
    html_url: 'https://github.com/78/xiaozhi-esp32/releases/tag/v2.5.2',
    ...overrides,
  };
}

function makeCandidate(
  overrides: Partial<XiaozhiFirmwareCandidate> = {},
): XiaozhiFirmwareCandidate {
  return {
    full_name: 'spotpear-sp-esp32-s3-1.28-box',
    board: 'spotpear/sp-esp32-s3-1.28-box',
    name: 'sp-esp32-s3-1.28-box',
    target: 'esp32s3',
    asset: 'v2.5.2_spotpear-sp-esp32-s3-1.28-box.zip',
    asset_size: 2_345_678,
    chip_match: true,
    score: 60,
    reason: '构建名匹配',
    ...overrides,
  };
}

describe('formatVersion', () => {
  it('缺少 v 前缀时补上，大小写统一为小写 v', () => {
    expect(formatVersion('2.5.2')).toBe('v2.5.2');
    expect(formatVersion('v2.5.2')).toBe('v2.5.2');
    expect(formatVersion('V2.5.2')).toBe('v2.5.2');
    expect(formatVersion('  2.5.2  ')).toBe('v2.5.2');
  });

  it('空值回退「未知版本」', () => {
    expect(formatVersion(undefined)).toBe('未知版本');
    expect(formatVersion(null)).toBe('未知版本');
    expect(formatVersion('')).toBe('未知版本');
    expect(formatVersion('   ')).toBe('未知版本');
  });
});

describe('formatReportTime', () => {
  it('毫秒时间戳格式化为本地 YYYY-MM-DD HH:mm', () => {
    const ms = new Date(2026, 9, 21, 18, 17, 30).getTime();
    expect(formatReportTime(ms)).toBe('2026-10-21 18:17');
  });

  it('缺省 / 非正数 / 非法值回退「未上报」', () => {
    expect(formatReportTime(undefined)).toBe('未上报');
    expect(formatReportTime(null)).toBe('未上报');
    expect(formatReportTime(0)).toBe('未上报');
    expect(formatReportTime(-1)).toBe('未上报');
    expect(formatReportTime(Number.NaN)).toBe('未上报');
  });
});

describe('formatFetchedAt', () => {
  it('ISO 字符串格式化为本地 YYYY-MM-DD HH:mm', () => {
    const iso = new Date(2026, 9, 21, 18, 17).toISOString();
    expect(formatFetchedAt(iso)).toBe('2026-10-21 18:17');
  });

  it('兼容毫秒时间戳字符串', () => {
    const ms = new Date(2026, 9, 21, 8, 5).getTime();
    expect(formatFetchedAt(String(ms))).toBe('2026-10-21 08:05');
  });

  it('空值回退「未知」，非法值原样返回', () => {
    expect(formatFetchedAt(undefined)).toBe('未知');
    expect(formatFetchedAt(null)).toBe('未知');
    expect(formatFetchedAt('  ')).toBe('未知');
    expect(formatFetchedAt('not-a-date')).toBe('not-a-date');
  });
});

describe('firmwareStateMeta', () => {
  it('7 种状态都有中文文案与 Badge class', () => {
    const states = Object.keys(FIRMWARE_STATE_META);
    expect(states).toHaveLength(7);
    expect(firmwareStateMeta('update_available').label).toBe('可升级');
    expect(firmwareStateMeta('latest').label).toBe('已是最新');
    expect(firmwareStateMeta('needs_selection').label).toBe('需选择');
    expect(firmwareStateMeta('ahead').label).toBe('领先上游');
    expect(firmwareStateMeta('unknown').label).toBe('待上报');
    expect(firmwareStateMeta('unsupported').label).toBe('不支持');
    expect(firmwareStateMeta('error').label).toBe('异常');
    for (const state of states) {
      expect(firmwareStateMeta(state as keyof typeof FIRMWARE_STATE_META).className).not.toBe('');
    }
  });
});

describe('candidateLabel', () => {
  it('组合构建名、芯片目标与匹配原因', () => {
    expect(candidateLabel(makeCandidate())).toBe('sp-esp32-s3-1.28-box · esp32s3 · 构建名匹配');
  });

  it('芯片不匹配时追加警告', () => {
    const label = candidateLabel(makeCandidate({ chip_match: false }));
    expect(label).toContain('sp-esp32-s3-1.28-box · esp32s3 · 构建名匹配');
    expect(label).toContain('⚠ 芯片不匹配');
  });

  it('name 缺失时回退 full_name，target/reason 缺失时省略', () => {
    expect(candidateLabel(makeCandidate({ name: '', target: '', reason: '' }))).toBe(
      'spotpear-sp-esp32-s3-1.28-box',
    );
    expect(candidateLabel(makeCandidate({ name: '', full_name: '' }))).toBe(
      '未知固件 · esp32s3 · 构建名匹配',
    );
  });
});

describe('releaseLabel', () => {
  it('组合版本与发布日期', () => {
    expect(releaseLabel(makeRelease())).toBe('v2.5.2 · 2026-10-11');
  });

  it('tag 缺失时回退 version', () => {
    expect(releaseLabel(makeRelease({ tag: '', version: '2.4.0' }))).toBe('v2.4.0 · 2026-10-11');
  });

  it('发布日期缺失或非法时只展示版本号', () => {
    expect(releaseLabel(makeRelease({ published_at: '' }))).toBe('v2.5.2');
    expect(releaseLabel(makeRelease({ published_at: 'not-a-date' }))).toBe('v2.5.2');
  });

  it('版本与日期都缺失时回退「未知版本」', () => {
    expect(releaseLabel(makeRelease({ tag: '', version: '', published_at: '' }))).toBe('未知版本');
  });
});

describe('defaultVersionForDevice', () => {
  const releases = [makeRelease({ tag: 'v2.5.2' }), makeRelease({ tag: 'v2.4.0' })];

  it('有 latest 时优先使用 latest.tag', () => {
    const latest = {
      tag: 'v2.5.2',
      version: '2.5.2',
      asset: 'xiaozhi-esp32-v2.5.2.zip',
      published_at: '2026-10-11T12:00:00Z',
      html_url: 'https://github.com/78/xiaozhi-esp32/releases/tag/v2.5.2',
    };
    expect(defaultVersionForDevice({ latest }, releases)).toBe('v2.5.2');
  });

  it('latest 为空时回退 releases[0].tag', () => {
    expect(defaultVersionForDevice({ latest: null }, releases)).toBe('v2.5.2');
    expect(defaultVersionForDevice(null, releases)).toBe('v2.5.2');
    expect(defaultVersionForDevice(undefined, releases)).toBe('v2.5.2');
  });

  it('没有 latest 也没有 releases 时返回空串', () => {
    expect(defaultVersionForDevice({ latest: null }, [])).toBe('');
    expect(defaultVersionForDevice(null, [])).toBe('');
  });
});

describe('describeFirmwareError', () => {
  const codes = [
    'invalid_request',
    'device_unknown',
    'release_not_found',
    'board_not_found',
    'board_ambiguous',
    'selection_required',
    'variant_not_found',
    'chip_mismatch',
    'asset_not_found',
    'download_failed',
    'checksum_mismatch',
    'extract_failed',
    'firmware_too_large',
    'cache_write_failed',
    'github_unavailable',
    'github_rate_limited',
    'upgrade_in_progress',
    'firmware_store_unavailable',
  ];

  it('每个后端错误码都映射成非通用提示', () => {
    for (const code of codes) {
      const message = describeFirmwareError(new XiaozhiError(code, 400, 'boom'));
      expect(message.length).toBeGreaterThan(0);
      expect(message.startsWith('操作失败（')).toBe(false);
    }
  });

  it('提示里带上后端 detail', () => {
    const error = new XiaozhiError('github_rate_limited', 429, 'rate limited', {
      detail: 'API limit exceeded',
    });
    expect(describeFirmwareError(error)).toContain('API limit exceeded');
  });

  it('selection_required 错误会携带候选列表', () => {
    const candidates = [makeCandidate(), makeCandidate({ full_name: 'other', chip_match: false })];
    const error = new XiaozhiError('selection_required', 409, 'selection required', {
      candidates,
    });
    expect(error.candidates).toEqual(candidates);
    expect(describeFirmwareError(error)).toContain('候选固件');
  });

  it('常见错误码给出可操作文案', () => {
    expect(describeFirmwareError(new XiaozhiError('github_unavailable', 502, 'x'))).toContain(
      '刷新版本信息',
    );
    expect(describeFirmwareError(new XiaozhiError('selection_required', 409, 'x'))).toContain(
      '候选固件',
    );
    expect(describeFirmwareError(new XiaozhiError('variant_not_found', 422, 'x'))).toContain(
      '重新选择候选固件',
    );
    expect(describeFirmwareError(new XiaozhiError('upgrade_in_progress', 409, 'x'))).toContain(
      '升级任务',
    );
    expect(
      describeFirmwareError(new XiaozhiError('firmware_store_unavailable', 500, 'x')),
    ).toContain('固件目录');
  });

  it('未知错误码与普通异常都给出兜底文案', () => {
    expect(describeFirmwareError(new XiaozhiError('weird_code', 500, 'boom'))).toBe(
      '操作失败（weird_code）：boom',
    );
    expect(describeFirmwareError(new Error('network down'))).toBe(
      '操作失败：请检查网络连接或 haimen 后端日志。',
    );
    expect(describeFirmwareError('oops')).toBe('操作失败：请检查网络连接或 haimen 后端日志。');
  });
});
