import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { DEFAULT_LOG_TAIL, openLogStream } from '@/api/logs';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import {
  accumulatePaused,
  appendCapped,
  extractLevel,
  filterLogLines,
  LOG_LEVELS,
  type LogLevelFilter,
} from '@/lib/logs';
import type { RuntimeLogMeta } from '@/types';

/** 页面最多保留的日志行数 */
const MAX_LINES = 2000;

type ConnectionState = 'connecting' | 'connected' | 'reconnecting' | 'disconnected';

const CONNECTION_META: Record<ConnectionState, { label: string; className: string }> = {
  connecting: { label: '连接中', className: 'bg-blue-500/15 text-blue-600 border-transparent' },
  connected: { label: '已连接', className: 'bg-green-500/15 text-green-600 border-transparent' },
  reconnecting: {
    label: '重连中',
    className: 'bg-yellow-500/15 text-yellow-600 border-transparent',
  },
  disconnected: {
    label: '已断开',
    className: 'bg-destructive/10 text-destructive border-transparent',
  },
};

/** 根据日志级别返回行的文字颜色 */
function levelClass(line: string): string {
  const level = extractLevel(line);
  if (level === 'ERROR') return 'text-destructive';
  if (level === 'WARN') return 'text-yellow-600 dark:text-yellow-500';
  if (level === 'DEBUG' || level === 'TRACE') return 'text-muted-foreground';
  return 'text-foreground';
}

export default function RuntimeLogs() {
  const [lines, setLines] = useState<string[]>([]);
  const [meta, setMeta] = useState<RuntimeLogMeta | null>(null);
  const [connection, setConnection] = useState<ConnectionState>('connecting');
  const [streamError, setStreamError] = useState<string | null>(null);
  const [paused, setPaused] = useState(false);
  const [pendingCount, setPendingCount] = useState(0);
  const [level, setLevel] = useState<LogLevelFilter>('ALL');
  const [keyword, setKeyword] = useState('');
  const [autoScroll, setAutoScroll] = useState(true);

  const pausedRef = useRef(false);
  const bufferRef = useRef<string[]>([]);
  const scrollRef = useRef<HTMLDivElement>(null);

  // 打开 SSE 流；卸载时关闭。EventSource 不可用或初始化失败时展示降级提示。
  useEffect(() => {
    let close: (() => void) | undefined;
    try {
      close = openLogStream({
        tail: DEFAULT_LOG_TAIL,
        onMeta: (nextMeta) => setMeta(nextMeta),
        onLines: (batch) => {
          if (batch.lines.length === 0) return;
          if (pausedRef.current) {
            bufferRef.current = accumulatePaused(bufferRef.current, batch.lines, MAX_LINES);
            setPendingCount(bufferRef.current.length);
            return;
          }
          setLines((prev) => appendCapped(prev, batch.lines, MAX_LINES));
        },
        onOpen: () => setConnection('connected'),
        onError: () => setConnection('reconnecting'),
      });
    } catch (err) {
      setConnection('disconnected');
      setStreamError(err instanceof Error ? err.message : '无法建立实时日志连接');
      return;
    }

    return () => {
      close?.();
    };
  }, []);

  const filteredLines = useMemo(
    () => filterLogLines(lines, { level, keyword }),
    [lines, level, keyword],
  );

  // 自动滚动：新行到达、筛选变化或重新开启时滚到底部
  useEffect(() => {
    if (!autoScroll || filteredLines.length === 0) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [autoScroll, filteredLines]);

  const handleResume = useCallback(() => {
    const buffered = bufferRef.current;
    bufferRef.current = [];
    setPendingCount(0);
    pausedRef.current = false;
    setPaused(false);
    if (buffered.length > 0) {
      setLines((prev) => appendCapped(prev, buffered, MAX_LINES));
    }
  }, []);

  const handleTogglePause = () => {
    if (paused) {
      handleResume();
      return;
    }
    pausedRef.current = true;
    setPaused(true);
  };

  const handleClear = () => {
    bufferRef.current = [];
    setPendingCount(0);
    setLines([]);
  };

  const connectionMeta = CONNECTION_META[connection];

  return (
    <div className="mx-auto max-w-6xl space-y-4 py-8 px-4">
      <div>
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="text-2xl font-bold tracking-tight">运行日志</h1>
          <Badge variant="outline" className={connectionMeta.className}>
            {connectionMeta.label}
          </Badge>
        </div>
        <p className="mt-1 text-sm text-muted-foreground">
          实时查看 haimen 应用日志，支持级别与关键字过滤。
        </p>
        <div className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
          <span className="max-w-full truncate font-mono" title={meta?.path}>
            {meta ? meta.path : '等待日志路径…'}
          </span>
          <span>共 {lines.length} 行</span>
          <span>
            显示 {filteredLines.length} / {lines.length} 行
          </span>
          {paused && (
            <span className="text-yellow-600 dark:text-yellow-500">
              已暂停，{pendingCount} 行待显示
            </span>
          )}
        </div>
      </div>

      {streamError && (
        <Alert variant="destructive">
          <AlertTitle>无法连接日志流</AlertTitle>
          <AlertDescription>{streamError}</AlertDescription>
        </Alert>
      )}

      <Card>
        <CardContent className="space-y-3 p-4">
          {/* 控制条 */}
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="outline" size="sm" onClick={handleTogglePause}>
              {paused ? '继续' : '暂停'}
            </Button>
            <Button
              variant="outline"
              size="sm"
              onClick={handleClear}
              disabled={lines.length === 0 && pendingCount === 0}
            >
              清空
            </Button>
            <select
              value={level}
              onChange={(e) => setLevel(e.target.value as LogLevelFilter)}
              aria-label="日志级别"
              className="h-8 cursor-pointer rounded-lg border border-input bg-transparent px-2.5 text-sm outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50"
            >
              <option value="ALL">全部</option>
              {LOG_LEVELS.map((item) => (
                <option key={item} value={item}>
                  {item}
                </option>
              ))}
            </select>
            <Input
              value={keyword}
              onChange={(e) => setKeyword(e.target.value)}
              placeholder="关键字过滤…"
              aria-label="关键字"
              className="h-8 w-[220px] text-xs"
            />
            <div className="ml-auto flex items-center gap-2 text-xs text-muted-foreground">
              <span>自动滚动</span>
              <Switch checked={autoScroll} onCheckedChange={setAutoScroll} aria-label="自动滚动" />
            </div>
          </div>

          {/* 日志面板 */}
          <div
            ref={scrollRef}
            className="h-[calc(100vh-380px)] min-h-[320px] overflow-auto rounded-lg border bg-muted/20 p-3 font-mono text-xs leading-relaxed"
          >
            {filteredLines.length === 0 ? (
              <div className="flex h-full items-center justify-center text-muted-foreground">
                {lines.length === 0 ? '等待日志输出…' : '没有匹配的日志行'}
              </div>
            ) : (
              filteredLines.map((line, index) => (
                // 日志行为追加式纯文本且内容可能重复，索引即展示顺序，作为 key 不会造成状态错位
                // biome-ignore lint/suspicious/noArrayIndexKey: 日志行按顺序展示，索引 key 足够稳定
                <div key={index} className={`whitespace-pre-wrap break-words ${levelClass(line)}`}>
                  {line || ' '}
                </div>
              ))
            )}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
