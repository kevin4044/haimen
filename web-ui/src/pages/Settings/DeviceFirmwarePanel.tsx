import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  clearXiaozhiFirmwareTarget,
  getXiaozhiFirmware,
  upgradeXiaozhiFirmware,
  XiaozhiError,
} from '@/api/xiaozhi';
import Combobox from '@/components/Combobox';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
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
import {
  defaultVersionForDevice,
  describeFirmwareError,
  firmwareStateMeta,
  formatFetchedAt,
  formatReportTime,
  formatVersion,
  releaseLabel,
} from '@/lib/firmware';
import type { XiaozhiFirmwareDevice, XiaozhiFirmwareOverview } from '@/types';

/** 上游 release 页面（与后端默认仓库一致；仅作跳转入口） */
const UPSTREAM_RELEASES_URL = 'https://github.com/78/xiaozhi-esp32/releases';

/** 设备展示名：名称为空或等于 MAC 时显示「未命名设备」 */
function deviceDisplayName(device: XiaozhiFirmwareDevice): string {
  const name = device.name?.trim();
  if (!name || name === device.device_id) return '未命名设备';
  return name;
}

/** 加载失败（网络 / 非 2xx）时的提示；XiaozhiError 用统一映射 */
function loadErrorMessage(error: unknown): string {
  if (error instanceof XiaozhiError) return describeFirmwareError(error);
  return '加载固件信息失败：请检查网络连接或 haimen 后端日志。';
}

/**
 * 固件升级（OTA）面板。
 *
 * 独立加载 `GET /api/v1/xiaozhi/firmware`，失败只影响本卡片，
 * 不会阻塞上面的设备配置表。升级 / 清除目标成功后重新拉取概览。
 */
export default function DeviceFirmwarePanel() {
  const [overview, setOverview] = useState<XiaozhiFirmwareOverview | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [clearingId, setClearingId] = useState<string | null>(null);

  const [dialogDevice, setDialogDevice] = useState<XiaozhiFirmwareDevice | null>(null);
  const [version, setVersion] = useState('');
  const [manualVersion, setManualVersion] = useState('');
  const [upgrading, setUpgrading] = useState(false);
  const [upgradeError, setUpgradeError] = useState<string | null>(null);

  /**
   * 拉取概览。`refresh=true` 强制后端重拉上游 release；
   * `silent=true` 时不切换整卡骨架屏（用于升级 / 清除后的静默重载）。
   */
  const loadOverview = useCallback(async (refresh = false, silent = false): Promise<void> => {
    if (refresh || silent) {
      setRefreshing(true);
    } else {
      setLoading(true);
    }
    if (!silent) setError(null);
    try {
      const data = await getXiaozhiFirmware(refresh);
      setOverview(data);
    } catch (loadError) {
      const message = loadErrorMessage(loadError);
      if (silent) {
        setActionError(message);
      } else {
        setError(message);
      }
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  }, []);

  useEffect(() => {
    void loadOverview();
  }, [loadOverview]);

  const devices = overview?.devices ?? [];
  const releaseOptions = useMemo(
    () =>
      (overview?.releases ?? []).map((release) => ({
        value: release.tag,
        label: releaseLabel(release),
      })),
    [overview],
  );

  const openUpgrade = (device: XiaozhiFirmwareDevice) => {
    setVersion(defaultVersionForDevice(device, overview?.releases ?? []));
    setManualVersion('');
    setUpgradeError(null);
    setDialogDevice(device);
  };

  const closeUpgrade = () => {
    if (upgrading) return;
    setDialogDevice(null);
    setUpgradeError(null);
  };

  const handleClearTarget = async (device: XiaozhiFirmwareDevice) => {
    setNotice(null);
    setActionError(null);
    setClearingId(device.device_id);
    try {
      await clearXiaozhiFirmwareTarget(device.device_id);
      setNotice(`已清除「${deviceDisplayName(device)}」的升级目标。`);
      await loadOverview(false, true);
    } catch (clearError) {
      setActionError(describeFirmwareError(clearError));
    } finally {
      setClearingId(null);
    }
  };

  const effectiveVersion = manualVersion.trim() || version.trim();

  const handleUpgrade = async () => {
    if (!dialogDevice) return;
    if (!effectiveVersion) {
      setUpgradeError('请选择或输入要升级到的版本。');
      return;
    }
    setUpgrading(true);
    setUpgradeError(null);
    setActionError(null);
    try {
      const result = await upgradeXiaozhiFirmware({
        device: dialogDevice.device_id,
        version: effectiveVersion,
      });
      const message = result.message?.trim() || '升级目标已设置';
      const suffix = message.includes('重启') ? '' : '（设备重启后生效）';
      setNotice(`已为「${deviceDisplayName(dialogDevice)}」设置升级目标：${message}${suffix}`);
      setDialogDevice(null);
      setManualVersion('');
      setVersion('');
      await loadOverview(false, true);
    } catch (upgradeFailure) {
      setUpgradeError(describeFirmwareError(upgradeFailure));
    } finally {
      setUpgrading(false);
    }
  };

  if (loading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">固件升级（OTA）</CardTitle>
        </CardHeader>
        <CardContent className="space-y-2">
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </CardContent>
      </Card>
    );
  }

  if (error || !overview) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">固件升级（OTA）</CardTitle>
        </CardHeader>
        <CardContent>
          <Alert variant="destructive">
            <AlertTitle>加载固件信息失败</AlertTitle>
            <AlertDescription>{error ?? '概览为空'}</AlertDescription>
          </Alert>
          <Button variant="outline" className="mt-2" onClick={() => void loadOverview()}>
            重试
          </Button>
        </CardContent>
      </Card>
    );
  }

  const releaseUrl = overview.repository_url
    ? `${overview.repository_url.replace(/\/+$/, '')}/releases`
    : UPSTREAM_RELEASES_URL;

  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="flex flex-wrap items-center gap-2 text-base">
          固件升级（OTA）
          <span className="text-xs font-normal text-muted-foreground">
            来源：{overview.repository || '78/xiaozhi-esp32'}
          </span>
        </CardTitle>
        <CardDescription>
          从上游 release 下载指定版本固件并设为升级目标；只更新 app
          分区，设备重启后生效。当前固件来自设备 OTA 上报。
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => void loadOverview(true, true)}
            disabled={refreshing}
          >
            {refreshing ? '刷新中...' : '刷新版本信息'}
          </Button>
          <a
            href={releaseUrl}
            target="_blank"
            rel="noreferrer"
            className="text-sm text-primary underline-offset-4 hover:underline"
          >
            上游 Release（GitHub）
          </a>
          {overview.fetched_at && (
            <span className="text-xs text-muted-foreground">
              版本信息获取于 {formatFetchedAt(overview.fetched_at)}
            </span>
          )}
        </div>

        {overview.stale && (
          <Alert className="border-yellow-500/50 bg-yellow-500/10">
            <AlertTitle className="text-yellow-700 dark:text-yellow-400">
              版本信息可能已过期
            </AlertTitle>
            <AlertDescription className="text-yellow-700/90 dark:text-yellow-400/90">
              上游 GitHub 暂时不可用，当前展示的是缓存数据。可以点击「刷新版本信息」重试。
            </AlertDescription>
          </Alert>
        )}

        {overview.error && (
          <Alert variant="destructive">
            <AlertTitle>上游版本获取失败</AlertTitle>
            <AlertDescription>
              {overview.error}
              <p className="mt-1">
                仍可在升级弹窗中手动输入版本；请检查网络或代理后点击「刷新版本信息」重试。
              </p>
            </AlertDescription>
          </Alert>
        )}

        {notice && (
          <Alert className="border-green-500/50 bg-green-500/10">
            <AlertTitle className="text-green-700 dark:text-green-400">操作成功</AlertTitle>
            <AlertDescription className="text-green-700/90 dark:text-green-400/90">
              {notice}
            </AlertDescription>
          </Alert>
        )}

        {actionError && (
          <Alert variant="destructive">
            <AlertTitle>操作失败</AlertTitle>
            <AlertDescription>{actionError}</AlertDescription>
          </Alert>
        )}

        {!overview.error && overview.releases.length === 0 && (
          <Alert>
            <AlertTitle>上游版本列表为空</AlertTitle>
            <AlertDescription>
              请点击「刷新版本信息」重试；也可以直接在升级弹窗中手动输入版本号。
            </AlertDescription>
          </Alert>
        )}

        {devices.length === 0 ? (
          <div className="rounded-lg border border-dashed p-8 text-center">
            <p className="text-sm text-muted-foreground">还没有可升级的设备</p>
            <p className="mt-1 text-xs text-muted-foreground">
              请先在「小智设备」中登记设备，并让设备启动后上报固件版本。
            </p>
          </div>
        ) : (
          <Table className="min-w-[52rem]" aria-label="小智设备固件状态">
            <TableHeader>
              <TableRow>
                <TableHead>设备</TableHead>
                <TableHead>当前固件</TableHead>
                <TableHead>板型</TableHead>
                <TableHead>状态</TableHead>
                <TableHead>目标</TableHead>
                <TableHead className="text-right">操作</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {devices.map((device) => {
                const meta = firmwareStateMeta(device.state);
                return (
                  <TableRow key={device.device_id}>
                    <TableCell>
                      <div className="flex flex-col gap-0.5">
                        <span className="font-medium">{deviceDisplayName(device)}</span>
                        <span className="text-xs text-muted-foreground">
                          {device.room?.trim() ? `房间：${device.room.trim()}` : '未设置房间'}
                        </span>
                        <span className="font-mono text-xs text-muted-foreground">
                          {device.device_id}
                        </span>
                      </div>
                    </TableCell>
                    <TableCell>
                      {device.report ? (
                        <div className="flex flex-col gap-0.5">
                          <span className="font-mono text-xs">
                            {formatVersion(device.report.version)}
                          </span>
                          <span className="text-xs text-muted-foreground">
                            上报于 {formatReportTime(device.report.reported_at_ms)}
                          </span>
                        </div>
                      ) : (
                        <span className="text-xs text-muted-foreground">未上报</span>
                      )}
                    </TableCell>
                    <TableCell>
                      {device.report ? (
                        <div className="flex flex-col gap-0.5">
                          <span className="text-xs">
                            {device.report.board_name || device.report.board_type || '未知板型'}
                          </span>
                          <span className="text-xs text-muted-foreground">
                            {device.report.chip_model || '芯片未知'}
                            {device.report.app_name ? ` · ${device.report.app_name}` : ''}
                          </span>
                        </div>
                      ) : (
                        <span className="text-xs text-muted-foreground">—</span>
                      )}
                    </TableCell>
                    <TableCell>
                      <div className="flex max-w-[12rem] flex-col items-start gap-1">
                        <Badge variant="outline" className={meta.className}>
                          {meta.label}
                        </Badge>
                        {device.latest && (
                          <span className="text-xs text-muted-foreground">
                            上游最新 {formatVersion(device.latest.tag || device.latest.version)}
                          </span>
                        )}
                        {device.error && (
                          <span className="text-xs text-destructive">{device.error}</span>
                        )}
                      </div>
                    </TableCell>
                    <TableCell>
                      {device.target ? (
                        <div className="flex flex-col gap-0.5">
                          <span className="font-mono text-xs">
                            {formatVersion(device.target.version)}
                          </span>
                          <span className="text-xs text-muted-foreground">
                            等待重启 · 已下发 {device.target.deliveries} 次
                          </span>
                          {!device.target.file_present && (
                            <span className="text-xs text-destructive">
                              固件文件缺失，请重新升级
                            </span>
                          )}
                        </div>
                      ) : (
                        <span className="text-xs text-muted-foreground">—</span>
                      )}
                    </TableCell>
                    <TableCell className="text-right">
                      <div className="flex flex-col items-end gap-1">
                        <div className="flex flex-wrap justify-end gap-1">
                          <Button
                            variant="outline"
                            size="sm"
                            disabled={!device.report}
                            aria-label={`升级 ${deviceDisplayName(device)} 的固件`}
                            onClick={() => openUpgrade(device)}
                          >
                            升级
                          </Button>
                          {device.target && (
                            <Button
                              variant="ghost"
                              size="sm"
                              disabled={clearingId === device.device_id}
                              aria-label={`清除 ${deviceDisplayName(device)} 的升级目标`}
                              onClick={() => void handleClearTarget(device)}
                            >
                              {clearingId === device.device_id ? '清除中...' : '清除目标'}
                            </Button>
                          )}
                        </div>
                        {!device.report && (
                          <span className="text-xs text-muted-foreground">
                            重启设备后上报固件版本
                          </span>
                        )}
                      </div>
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        )}
      </CardContent>

      <Dialog open={dialogDevice !== null} onOpenChange={(open) => !open && closeUpgrade()}>
        {dialogDevice && (
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle>固件升级（OTA）</DialogTitle>
              <DialogDescription>
                为「{deviceDisplayName(dialogDevice)}」指定要刷入的固件版本。
              </DialogDescription>
            </DialogHeader>

            <div className="space-y-3">
              <div className="space-y-1.5">
                <span className="text-sm font-medium">版本</span>
                <Combobox
                  options={releaseOptions}
                  value={version || null}
                  onChange={setVersion}
                  placeholder="选择上游版本"
                  searchPlaceholder="搜索版本..."
                  emptyText="没有匹配的版本，可在下方手动输入"
                />
              </div>

              <div className="space-y-1.5">
                <label className="text-sm font-medium" htmlFor="firmware-manual-version">
                  或手动输入版本（如 v2.4.0）
                </label>
                <Input
                  id="firmware-manual-version"
                  aria-label="手动输入固件版本"
                  placeholder="v2.4.0"
                  className="font-mono"
                  value={manualVersion}
                  onChange={(event) => {
                    setManualVersion(event.target.value);
                    setUpgradeError(null);
                  }}
                />
                <p className="text-xs text-muted-foreground">
                  非空时覆盖上方选择；留空则使用所选版本
                  {effectiveVersion ? `（将使用 ${effectiveVersion}）` : ''}。
                </p>
              </div>

              <Alert className="border-yellow-500/50 bg-yellow-500/10">
                <AlertTitle className="text-yellow-700 dark:text-yellow-400">升级前须知</AlertTitle>
                <AlertDescription className="text-yellow-700/90 dark:text-yellow-400/90">
                  将刷入上游固件，覆盖当前应用分区；设备需重启后生效；只更新 app 分区。
                </AlertDescription>
              </Alert>

              {upgradeError && (
                <Alert variant="destructive">
                  <AlertTitle>升级失败</AlertTitle>
                  <AlertDescription>{upgradeError}</AlertDescription>
                </Alert>
              )}
            </div>

            <DialogFooter>
              <Button variant="outline" onClick={closeUpgrade} disabled={upgrading}>
                取消
              </Button>
              <Button onClick={() => void handleUpgrade()} disabled={upgrading}>
                {upgrading ? '下发中...' : '确认升级'}
              </Button>
            </DialogFooter>
          </DialogContent>
        )}
      </Dialog>
    </Card>
  );
}
