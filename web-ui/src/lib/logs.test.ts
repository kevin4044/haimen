import { describe, expect, it } from 'vitest';
import { accumulatePaused, appendCapped, extractLevel, filterLogLines, LEVEL_RE } from './logs';

describe('extractLevel', () => {
  it('识别标准 tracing 日志行中的级别', () => {
    expect(extractLevel('2026-10-20T12:00:00.123Z  INFO haimen::web: 服务已启动')).toBe('INFO');
    expect(extractLevel('2026-10-20T12:00:00.123Z  WARN haimen::web: 即将超时')).toBe('WARN');
    expect(extractLevel('2026-10-20T12:00:00.123Z ERROR haimen::web: 连接失败')).toBe('ERROR');
    expect(extractLevel('2026-10-20T12:00:00.123Z DEBUG haimen::web: 调试信息')).toBe('DEBUG');
    expect(extractLevel('2026-10-20T12:00:00.123Z TRACE haimen::web: 追踪信息')).toBe('TRACE');
  });

  it('大小写不敏感', () => {
    expect(extractLevel('info 小写级别')).toBe('INFO');
    expect(extractLevel('Warn 混合大小写')).toBe('WARN');
    expect(extractLevel('eRrOr 混合大小写')).toBe('ERROR');
  });

  it('要求整词匹配', () => {
    expect(extractLevel('WARNING：磁盘空间不足')).toBeNull();
    expect(extractLevel('INFORMATION：仅提示')).toBeNull();
    expect(extractLevel('DEBUGGER：调试器')).toBeNull();
    expect(extractLevel('ERROR_CODE=500')).toBeNull();
  });

  it('没有级别标记时返回 null', () => {
    expect(extractLevel('一条没有级别的日志')).toBeNull();
    expect(extractLevel('')).toBeNull();
    expect(extractLevel('request_id=INFO123')).toBeNull();
  });

  it('LEVEL_RE 可重复使用且大小写不敏感', () => {
    expect(LEVEL_RE.test('info')).toBe(true);
    expect(LEVEL_RE.test('ERROR')).toBe(true);
    expect(LEVEL_RE.test('nothing')).toBe(false);
  });
});

describe('filterLogLines', () => {
  const lines = [
    '2026-10-20T12:00:00Z  INFO haimen::web: 服务已启动',
    '2026-10-20T12:00:01Z  WARN haimen::web: 磁盘空间不足',
    '2026-10-20T12:00:02Z ERROR haimen::web: 连接失败',
    '2026-10-20T12:00:03Z DEBUG haimen::web: 收到请求',
    '无级别的一行日志',
  ];

  it('级别为 ALL 时返回全部行并保持顺序', () => {
    expect(filterLogLines(lines, { level: 'ALL', keyword: '' })).toEqual(lines);
  });

  it('按级别过滤（大小写不敏感的级别提取）', () => {
    expect(filterLogLines(lines, { level: 'ERROR', keyword: '' })).toEqual([
      '2026-10-20T12:00:02Z ERROR haimen::web: 连接失败',
    ]);
    expect(filterLogLines(lines, { level: 'INFO', keyword: '' })).toHaveLength(1);
  });

  it('关键字大小写不敏感且为子串匹配', () => {
    expect(filterLogLines(lines, { level: 'ALL', keyword: 'haimen::WEB' })).toHaveLength(4);
    expect(filterLogLines(lines, { level: 'ALL', keyword: '失败' })).toEqual([
      '2026-10-20T12:00:02Z ERROR haimen::web: 连接失败',
    ]);
  });

  it('关键字首尾空白会被忽略', () => {
    expect(filterLogLines(lines, { level: 'ALL', keyword: '  失败  ' })).toHaveLength(1);
    expect(filterLogLines(lines, { level: 'ALL', keyword: '   ' })).toEqual(lines);
  });

  it('级别与关键字同时生效', () => {
    expect(filterLogLines(lines, { level: 'WARN', keyword: '磁盘' })).toEqual([
      '2026-10-20T12:00:01Z  WARN haimen::web: 磁盘空间不足',
    ]);
    expect(filterLogLines(lines, { level: 'ERROR', keyword: '磁盘' })).toEqual([]);
  });

  it('级别过滤会排除无法识别级别的行', () => {
    expect(filterLogLines(['无级别'], { level: 'INFO', keyword: '' })).toEqual([]);
  });

  it('没有匹配行时返回空数组', () => {
    expect(filterLogLines(lines, { level: 'ALL', keyword: '不存在的关键字' })).toEqual([]);
    expect(filterLogLines([], { level: 'ERROR', keyword: '' })).toEqual([]);
  });
});

describe('appendCapped', () => {
  it('未超过上限时按顺序合并', () => {
    expect(appendCapped(['a', 'b'], ['c', 'd'], 10)).toEqual(['a', 'b', 'c', 'd']);
  });

  it('超过上限时保留最后 max 行', () => {
    expect(appendCapped(['a', 'b', 'c'], ['d', 'e'], 3)).toEqual(['c', 'd', 'e']);
  });

  it('单批新行就超过上限时只保留最后 max 行', () => {
    expect(appendCapped([], ['1', '2', '3', '4', '5'], 2)).toEqual(['4', '5']);
  });

  it('max <= 0 时返回空数组', () => {
    expect(appendCapped(['a'], ['b'], 0)).toEqual([]);
    expect(appendCapped(['a'], ['b'], -3)).toEqual([]);
  });

  it('不修改入参数组', () => {
    const existing = ['a', 'b'];
    const incoming = ['c', 'd'];
    const result = appendCapped(existing, incoming, 3);
    expect(result).toEqual(['b', 'c', 'd']);
    expect(existing).toEqual(['a', 'b']);
    expect(incoming).toEqual(['c', 'd']);
  });

  it('空 incoming 返回原内容的副本', () => {
    const existing = ['a', 'b'];
    const result = appendCapped(existing, [], 5);
    expect(result).toEqual(['a', 'b']);
    expect(result).not.toBe(existing);
  });
});

describe('accumulatePaused（暂停语义）', () => {
  it('暂停期间新行累积到缓冲区并保持顺序', () => {
    let buffer = accumulatePaused([], ['a', 'b'], 5);
    buffer = accumulatePaused(buffer, ['c'], 5);
    expect(buffer).toEqual(['a', 'b', 'c']);
  });

  it('缓冲区同样受上限约束', () => {
    let buffer = accumulatePaused([], ['1', '2', '3'], 4);
    buffer = accumulatePaused(buffer, ['4', '5'], 4);
    expect(buffer).toEqual(['2', '3', '4', '5']);
  });

  it('恢复时按“已有行 + 缓冲行”顺序合并并重新裁剪', () => {
    const visible = ['old-1', 'old-2'];
    const buffer = accumulatePaused([], ['new-1', 'new-2', 'new-3'], 10);
    expect(appendCapped(visible, buffer, 4)).toEqual(['old-2', 'new-1', 'new-2', 'new-3']);
  });

  it('无待显示行时恢复不改变已有行', () => {
    expect(appendCapped(['visible'], [], 10)).toEqual(['visible']);
  });
});
