import { useCallback, useEffect, useState } from 'react';
import { ApiError } from '@/api/client';
import * as xiaozhiApi from '@/api/xiaozhi';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import type { XiaozhiDeviceState, XiaozhiOnlineDevice } from '@/types';
import DeviceFirmwarePanel from './DeviceFirmwarePanel';

/** 编辑中的一行设备（key 只用于 React 列表渲染，不提交给后端） */
interface EditableDevice {
  key: string;
  device_id: string;
  name: string;
  room: string;
}

let rowSeed = 0;

function nextRowKey(): string {
  rowSeed += 1;
  return `device-row-${rowSeed}`;
}

/** 把 MAC 各种写法统一成冒号小写形式（与后端 normalize_mac 对齐） */
function normalizeMac(raw: string): string {
  const mac = raw.trim().toLowerCase();
  if (mac.includes(':')) return mac;
  if (mac.includes('_')) return mac.replaceAll('_', ':');
  if (/^[0-9a-f]{12}$/.test(mac)) return mac.match(/.{2}/g)?.join(':') ?? mac;
  return mac;
}

/** 归一化设备身份：MQTT client_id 先取出 `@@@` 之间的 MAC 段，与后端保持一致 */
function normalizeDeviceId(raw: string): string {
  const value = raw.trim();
  if (value.includes('@@@')) {
    const parts = value.split('@@@').filter(Boolean);
    return normalizeMac(parts[1] ?? value);
  }
  return normalizeMac(value);
}

const STATE_META: Record<XiaozhiDeviceState, { label: string; className: string }> = {
  idle: { label: '空闲', className: 'bg-green-500/15 text-green-600 border-transparent' },
  listening: {
    label: '聆听中',
    className: 'bg-yellow-500/15 text-yellow-600 border-transparent',
  },
  playing: { label: '播放中', className: 'bg-primary/10 text-primary border-transparent' },
};

function StateBadge({ state }: { state: XiaozhiDeviceState }) {
  const meta = STATE_META[state];
  return (
    <Badge variant="outline" className={meta.className}>
      {meta.label}
    </Badge>
  );
}

function OnlineBadge({ online }: { online: boolean }) {
  return online ? (
    <Badge variant="outline" className="bg-green-500/15 text-green-600 border-transparent">
      在线
    </Badge>
  ) : (
    <Badge variant="outline" className="text-muted-foreground">
      离线
    </Badge>
  );
}

function DeviceSettingsPanel() {
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [rows, setRows] = useState<EditableDevice[]>([]);
  const [online, setOnline] = useState<XiaozhiOnlineDevice[]>([]);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saveResult, setSaveResult] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await xiaozhiApi.getXiaozhiDevices();
      setRows(
        data.devices.map((device) => ({
          key: nextRowKey(),
          device_id: device.device_id,
          name: device.name,
          room: device.room,
        })),
      );
      setOnline(data.online);
    } catch {
      setError('加载小智设备失败');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const updateRow = (key: string, field: 'device_id' | 'name' | 'room', value: string) => {
    setRows((prev) => prev.map((row) => (row.key === key ? { ...row, [field]: value } : row)));
    setSaveError(null);
    setSaveResult(null);
  };

  const addRow = () => {
    setRows((prev) => [...prev, { key: nextRowKey(), device_id: '', name: '', room: '' }]);
    setSaveError(null);
    setSaveResult(null);
  };

  const removeRow = (key: string) => {
    setRows((prev) => prev.filter((row) => row.key !== key));
    setSaveError(null);
    setSaveResult(null);
  };

  /** 在线但还没登记的设备：一键填入表格，保存后即纳入设备目录 */
  const registerOnline = (device: XiaozhiOnlineDevice) => {
    setRows((prev) => [
      ...prev,
      {
        key: nextRowKey(),
        device_id: device.device_id,
        // 未登记设备的 name 会回退成 MAC，填入表格时还原为空更直观
        name: device.name === device.device_id ? '' : device.name,
        room: device.room,
      },
    ]);
    setSaveError(null);
    setSaveResult(null);
  };

  const handleSave = async () => {
    setSaveError(null);
    setSaveResult(null);

    // 前端先做一遍校验，给出比后端 400 更具体的定位信息
    const payload: xiaozhiApi.XiaozhiDeviceInput[] = [];
    const seen = new Set<string>();
    for (const [index, row] of rows.entries()) {
      const deviceId = row.device_id.trim();
      if (!deviceId) {
        setSaveError(`第 ${index + 1} 行的设备 ID 不能为空`);
        return;
      }
      const mac = normalizeDeviceId(deviceId);
      if (seen.has(mac)) {
        setSaveError(`设备 ID 重复：${mac}`);
        return;
      }
      seen.add(mac);
      payload.push({ device_id: deviceId, name: row.name.trim(), room: row.room.trim() });
    }

    setSaving(true);
    try {
      await xiaozhiApi.updateXiaozhiDevices(payload);
      // 保存后重新加载：后端会返回归一化 MAC，保证与在线列表按同一身份对齐
      await load();
      setSaveResult('保存成功，设备目录已即时生效');
    } catch (e) {
      if (e instanceof ApiError && e.status === 400) {
        setSaveError('保存失败：设备 ID 不能为空或重复');
      } else {
        setSaveError('保存失败，请检查网络或后端日志');
      }
    } finally {
      setSaving(false);
    }
  };

  if (loading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">小智设备</CardTitle>
        </CardHeader>
        <CardContent className="space-y-2">
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </CardContent>
      </Card>
    );
  }

  if (error) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">小智设备</CardTitle>
        </CardHeader>
        <CardContent>
          <Alert variant="destructive">
            <AlertTitle>加载失败</AlertTitle>
            <AlertDescription>{error}</AlertDescription>
          </Alert>
          <Button variant="outline" className="mt-2" onClick={load}>
            重试
          </Button>
        </CardContent>
      </Card>
    );
  }

  // 已配置设备的归一化 MAC 集合：决定在线列表里哪些设备属于「未登记」
  const configuredMacs = new Set(
    rows.map((row) => normalizeDeviceId(row.device_id)).filter((mac) => mac !== ''),
  );
  const onlineMap = new Map(online.map((device) => [normalizeDeviceId(device.device_id), device]));
  const unregistered = online.filter(
    (device) => !configuredMacs.has(normalizeDeviceId(device.device_id)),
  );
  const onlineCount = online.length;

  return (
    <div className="space-y-6">
      <Alert>
        <AlertTitle>说明</AlertTitle>
        <AlertDescription>
          <p>房间会注入 LLM 上下文：例如「客厅」的设备回答时会知道自己在客厅。</p>
          <p>通知可按设备名或房间下发；保存后即时生效，无需重启。</p>
        </AlertDescription>
      </Alert>

      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2 text-base">
            已配置设备
            <span className="text-xs font-normal text-muted-foreground">
              {rows.length} 台 / 在线 {onlineCount} 台
            </span>
          </CardTitle>
          <CardDescription>
            设备身份支持 MAC（冒号/下划线/12 位连写）或 MQTT client_id；名称留空时显示 MAC。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {rows.length === 0 ? (
            <div className="rounded-lg border border-dashed p-8 text-center">
              <p className="text-sm text-muted-foreground">还没有配置设备</p>
              <p className="mt-1 text-xs text-muted-foreground">
                点击「新增设备」手动填写 MAC，或在下方把在线设备一键登记。
              </p>
              <Button variant="outline" className="mt-4" onClick={addRow}>
                新增设备
              </Button>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-24">状态</TableHead>
                  <TableHead>设备 ID</TableHead>
                  <TableHead>名称</TableHead>
                  <TableHead>房间</TableHead>
                  <TableHead className="w-16 text-right">操作</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((row) => {
                  const mac = normalizeDeviceId(row.device_id);
                  const info = onlineMap.get(mac);
                  return (
                    <TableRow key={row.key}>
                      <TableCell>
                        <div className="flex flex-col items-start gap-1">
                          <OnlineBadge online={Boolean(info)} />
                          {info && <StateBadge state={info.state} />}
                        </div>
                      </TableCell>
                      <TableCell>
                        <Input
                          aria-label="设备 ID（MAC 或 client_id）"
                          placeholder="84:c7:bb:59:93:64"
                          value={row.device_id}
                          onChange={(e) => updateRow(row.key, 'device_id', e.target.value)}
                          className="font-mono"
                        />
                      </TableCell>
                      <TableCell>
                        <Input
                          aria-label="设备名称"
                          placeholder="如：客厅小智"
                          value={row.name}
                          onChange={(e) => updateRow(row.key, 'name', e.target.value)}
                        />
                      </TableCell>
                      <TableCell>
                        <Input
                          aria-label="所在房间"
                          placeholder="如：客厅"
                          value={row.room}
                          onChange={(e) => updateRow(row.key, 'room', e.target.value)}
                        />
                      </TableCell>
                      <TableCell className="text-right">
                        <Button variant="destructive" size="xs" onClick={() => removeRow(row.key)}>
                          删除
                        </Button>
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          )}

          {unregistered.length > 0 && (
            <div className="rounded-lg border p-3">
              <p className="text-sm font-medium">在线但未登记的设备</p>
              <p className="mt-0.5 text-xs text-muted-foreground">
                这些设备已连上 MQTT 传输层，但不在设备表里，不会注入房间上下文。
              </p>
              <ul className="mt-2 space-y-1">
                {unregistered.map((device) => (
                  <li
                    key={device.client_id}
                    className="flex items-center justify-between gap-2 text-sm"
                  >
                    <span className="flex min-w-0 items-center gap-2">
                      <span className="truncate font-mono text-xs">{device.device_id}</span>
                      <StateBadge state={device.state} />
                    </span>
                    <Button variant="outline" size="xs" onClick={() => registerOnline(device)}>
                      登记
                    </Button>
                  </li>
                ))}
              </ul>
            </div>
          )}

          <div className="flex flex-wrap items-center gap-2">
            <Button variant="outline" onClick={addRow}>
              新增设备
            </Button>
            <Button onClick={handleSave} disabled={saving}>
              {saving ? '保存中...' : '保存配置'}
            </Button>
            <Button variant="ghost" onClick={load} disabled={saving}>
              重新加载
            </Button>
            {saveResult && <span className="text-sm text-green-600">{saveResult}</span>}
            {saveError && <span className="text-sm text-destructive">{saveError}</span>}
          </div>
        </CardContent>
      </Card>

      {/* 固件面板独立加载概览；加载失败只影响本卡片，不影响上面的设备表 */}
      <DeviceFirmwarePanel />
    </div>
  );
}

// ─── 页面 ───────────────────────────────────────────────────

export default function DeviceSettings() {
  return (
    <div className="mx-auto max-w-4xl space-y-8 py-8 px-4">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">小智设备</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          登记多台小智设备的名称与房间，查看当前在线状态与忙闲。
        </p>
      </div>
      <DeviceSettingsPanel />
    </div>
  );
}
