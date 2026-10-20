import type { ReactNode } from 'react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { ApiError } from '@/api/client';
import {
  getXiaozhiControlDevices,
  getXiaozhiSettings,
  sayXiaozhi,
  updateXiaozhiSettings,
  type XiaozhiControlDevice,
  XiaozhiError,
} from '@/api/xiaozhi';
import Combobox from '@/components/Combobox';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import type { XiaozhiMqttSettings, XiaozhiSayResult, XiaozhiSettings } from '@/types';

// ─── 小工具 ─────────────────────────────────────────────────

type TargetMode = 'auto' | 'device' | 'room';

type FieldKey =
  | 'endpoint'
  | 'keepalive'
  | 'publishTopic'
  | 'downlinkTopic'
  | 'proxies'
  | 'sayText'
  | 'sayTarget';

type FieldErrors = Partial<Record<FieldKey, string>>;

const PROTOCOL_OPTIONS = [
  { value: 'websocket', label: 'WebSocket（默认，无需 Broker）' },
  { value: 'mqtt', label: 'MQTT（多设备 / 房间 / 主动播报）' },
];

const TARGET_MODE_OPTIONS = [
  { value: 'auto', label: '自动（唯一在线设备）' },
  { value: 'device', label: '指定设备' },
  { value: 'room', label: '指定房间' },
];

const DEVICE_STATE_LABEL: Record<'idle' | 'listening' | 'playing', string> = {
  idle: '空闲',
  listening: '聆听中',
  playing: '播放中',
};

/** 文本的字符数（按码点，避免 emoji 被算成两个字符） */
function charCount(text: string): number {
  return Array.from(text).length;
}

/** 简单 IP 形态校验：IPv4 四段 ≤255，IPv6 只做字符/长度检查（后端权威） */
function isIpLike(value: string): boolean {
  if (value.length === 0 || value.length > 45) return false;
  if (value.includes(':')) {
    return /^[0-9a-fA-F:.]+$/.test(value);
  }
  const parts = value.split('.');
  if (parts.length !== 4) return false;
  return parts.every((part) => /^\d{1,3}$/.test(part) && Number(part) <= 255);
}

/** 逗号 / 换行分隔的代理列表；返回去空行后的原始值与非法项 */
function parseProxyLines(raw: string): { proxies: string[]; invalid: string[] } {
  const proxies = raw
    .split(/[\n,，]+/)
    .map((part) => part.trim())
    .filter((part) => part.length > 0);
  return { proxies, invalid: proxies.filter((part) => !isIpLike(part)) };
}

function deviceLabel(device: XiaozhiControlDevice): string {
  const mac = device.device_id;
  const name = device.name?.trim();
  const displayName = name && name !== mac ? name : '未命名设备';
  const room = device.room?.trim() ? ` · ${device.room.trim()}` : '';
  const state =
    device.state && device.state !== 'idle' ? ` · ${DEVICE_STATE_LABEL[device.state]}` : '';
  return `${displayName}${room} · ${mac}${state}`;
}

function deviceValue(device: XiaozhiControlDevice): string {
  return device.client_id?.trim() || device.device_id;
}

function describeSayError(error: unknown): string {
  if (!(error instanceof XiaozhiError)) {
    return '请求失败：请检查网络连接或 haimen 后端日志。';
  }
  const detail = error.detail ? `（${error.detail}）` : '';
  switch (error.code) {
    case 'unauthorized':
      return `鉴权失败：say_token 不匹配${detail}。请确认「主动播报」里的 Token 与 haimen 配置一致，并先保存。`;
    case 'device_offline':
      return `设备离线：没有找到可下发的设备${error.room ? `（房间：${error.room}）` : ''}。请确认 haimen 运行在 MQTT 模式、设备已启动 MQTT 并在线${detail}。`;
    case 'device_busy':
      return `设备正忙：当前状态为 ${error.state ?? 'listening/playing'}，等这次对话结束后再试。`;
    case 'multiple_devices': {
      const candidates = (error.devices ?? [])
        .map((device) => {
          const room = device.room?.trim() ? `（${device.room.trim()}）` : '';
          return `${device.name || device.device_id}${room}`;
        })
        .join('、');
      return `命中多台设备，请在上方「目标」中明确选择一台设备或房间${candidates ? `。候选：${candidates}` : ''}。`;
    }
    case 'tts_failed':
      return `TTS 合成失败：请检查「语音配置」里的合成服务与网络${detail}。`;
    case 'bad_request':
      return `请求不合法：播报内容为空或超过 500 字${detail}。`;
    default:
      return `下发失败（${error.code}）：${error.message}`;
  }
}

function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return '未知';
  if (ms < 1000) return `${ms} ms`;
  return `${(ms / 1000).toFixed(1)} 秒`;
}

// ─── 展示组件 ───────────────────────────────────────────────

function Field({
  label,
  hint,
  error,
  children,
}: {
  label: string;
  hint?: ReactNode;
  error?: string;
  children: ReactNode;
}) {
  return (
    <div className="space-y-1.5">
      <span className="text-sm font-medium">{label}</span>
      {children}
      {error ? (
        <p className="text-xs text-destructive">{error}</p>
      ) : (
        hint && <p className="text-xs text-muted-foreground">{hint}</p>
      )}
    </div>
  );
}

function SwitchRow({
  label,
  description,
  checked,
  onCheckedChange,
}: {
  label: string;
  description?: string;
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
}) {
  return (
    <div className="flex items-start justify-between gap-4 rounded-lg border p-3">
      <div className="space-y-0.5">
        <p className="text-sm font-medium">{label}</p>
        {description && <p className="text-xs text-muted-foreground">{description}</p>}
      </div>
      <Switch checked={checked} onCheckedChange={onCheckedChange} />
    </div>
  );
}

function WarningAlert({ title, children }: { title: string; children: ReactNode }) {
  return (
    <Alert className="border-yellow-500/50 bg-yellow-500/10">
      <AlertTitle className="text-yellow-700 dark:text-yellow-400">{title}</AlertTitle>
      <AlertDescription className="text-yellow-700/90 dark:text-yellow-400/90">
        {children}
      </AlertDescription>
    </Alert>
  );
}

// ─── 页面 ───────────────────────────────────────────────────

export default function XiaozhiSettingsPage() {
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [settings, setSettings] = useState<XiaozhiSettings | null>(null);
  const [proxiesText, setProxiesText] = useState('');
  /** 已保存的 say_token：测试与设备清单都用它，避免用未保存的草稿值鉴权 */
  const [savedSayToken, setSavedSayToken] = useState('');

  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saveResult, setSaveResult] = useState<string | null>(null);
  const [restartRequired, setRestartRequired] = useState(false);
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>({});

  const [devices, setDevices] = useState<XiaozhiControlDevice[]>([]);
  const [devicesLoading, setDevicesLoading] = useState(false);
  const [devicesError, setDevicesError] = useState<string | null>(null);
  const [mqttRunning, setMqttRunning] = useState<boolean | null>(null);

  const [targetMode, setTargetMode] = useState<TargetMode>('auto');
  const [targetDevice, setTargetDevice] = useState('');
  const [targetRoom, setTargetRoom] = useState('');
  const [sayText, setSayText] = useState('');
  const [sending, setSending] = useState(false);
  const [sayResult, setSayResult] = useState<XiaozhiSayResult | null>(null);
  const [sayError, setSayError] = useState<string | null>(null);
  const [copiedKey, setCopiedKey] = useState<string | null>(null);

  const applyLoadedSettings = useCallback((next: XiaozhiSettings) => {
    setSettings(next);
    setProxiesText(next.ota_trusted_proxies.join('\n'));
    setSavedSayToken(next.say_token);
  }, []);

  const loadDevices = useCallback(async (token: string) => {
    setDevicesLoading(true);
    setDevicesError(null);
    try {
      const result = await getXiaozhiControlDevices(token);
      setDevices(result.devices);
      setMqttRunning(result.mqttRunning);
    } catch (error) {
      setDevices([]);
      // 拉取失败时不再展示上一次的 MQTT 运行状态，避免误导
      setMqttRunning(null);
      if (error instanceof XiaozhiError && error.code === 'unauthorized') {
        setDevicesError('say_token 不匹配，无法读取在线设备。请检查下方 Token 并先保存。');
      } else if (error instanceof XiaozhiError) {
        setDevicesError(`读取在线设备失败：${error.message}`);
      } else {
        setDevicesError('读取在线设备失败：请确认 haimen 服务可达、MQTT 传输层已启动。');
      }
    } finally {
      setDevicesLoading(false);
    }
  }, []);

  const loadSettings = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      const loaded = await getXiaozhiSettings();
      applyLoadedSettings(loaded);
      // 只加载一次设备；之后靠「刷新设备」按需拉取，避免无意义轮询
      void loadDevices(loaded.say_token);
    } catch {
      setLoadError('加载小智配置失败');
    } finally {
      setLoading(false);
    }
  }, [applyLoadedSettings, loadDevices]);

  useEffect(() => {
    void loadSettings();
  }, [loadSettings]);

  /** 编辑任意字段都清掉上一次的保存反馈 */
  const markDirty = () => {
    setFieldErrors({});
    setSaveError(null);
    setSaveResult(null);
    setRestartRequired(false);
  };

  const patchSettings = (updater: (prev: XiaozhiSettings) => XiaozhiSettings) => {
    setSettings((prev) => (prev ? updater(prev) : prev));
    markDirty();
  };

  const patchMqtt = <K extends keyof XiaozhiMqttSettings>(
    key: K,
    value: XiaozhiMqttSettings[K],
  ) => {
    patchSettings((prev) => ({ ...prev, mqtt: { ...prev.mqtt, [key]: value } }));
  };

  const buildPayload = (): XiaozhiSettings | null => {
    if (!settings) return null;

    const errors: FieldErrors = {};
    const endpoint = settings.mqtt.endpoint.trim();
    if (settings.protocol === 'mqtt' && !endpoint) {
      errors.endpoint = 'MQTT 模式下 Broker 地址不能为空，例如 192.168.1.10:1883';
    }
    if (!settings.mqtt.publish_topic_template.includes('{mac}')) {
      errors.publishTopic = '上行主题模板必须包含 {mac} 占位符';
    }
    if (!settings.mqtt.downlink_topic_template.includes('{mac}')) {
      errors.downlinkTopic = '下行主题模板必须包含 {mac} 占位符';
    }
    if (!Number.isInteger(settings.mqtt.keepalive) || settings.mqtt.keepalive < 1) {
      errors.keepalive = 'Keepalive 必须是大于等于 1 的整数（秒）';
    }
    const parsedProxies = parseProxyLines(proxiesText);
    if (parsedProxies.invalid.length > 0) {
      errors.proxies = `以下内容不是合法 IP：${parsedProxies.invalid.join('、')}`;
    }

    setFieldErrors(errors);
    if (Object.keys(errors).length > 0) {
      setSaveError('请先修正标红的配置项');
      setSaveResult(null);
      return null;
    }

    return {
      ...settings,
      mqtt: {
        ...settings.mqtt,
        endpoint,
        publish_topic_template: settings.mqtt.publish_topic_template.trim(),
        downlink_topic_template: settings.mqtt.downlink_topic_template.trim(),
      },
      udp: { bind_addr: settings.udp.bind_addr.trim() },
      ota_trusted_proxies: parsedProxies.proxies,
    };
  };

  const handleSave = async () => {
    const payload = buildPayload();
    if (!payload) return;

    setSaving(true);
    setSaveError(null);
    setSaveResult(null);
    setRestartRequired(false);
    try {
      const result = await updateXiaozhiSettings(payload);
      // 保存后重新读取，展示后端归一化后的值；GET 失败时回退到 PUT 返回的 data
      let next = result.settings;
      try {
        next = await getXiaozhiSettings();
      } catch {
        // 忽略：PUT 已经成功，用它的 data 回填
      }
      const tokenChanged = next.say_token !== savedSayToken;
      applyLoadedSettings(next);
      // 需要重启时服务端 message 已由下方黄色 Alert 展示，这里只报写入成功
      setSaveResult(
        result.restartRequired ? '配置已写入 settings.toml' : result.message || '保存成功',
      );
      setRestartRequired(result.restartRequired);
      // say_token 变了要按新 token 重新拉设备清单
      if (tokenChanged) void loadDevices(next.say_token);
    } catch (error) {
      if (error instanceof ApiError) {
        setSaveError(`保存失败：${error.message || '请检查网络或后端日志'}`);
      } else {
        setSaveError('保存失败：请检查网络或后端日志');
      }
    } finally {
      setSaving(false);
    }
  };

  const handleSay = async () => {
    if (!settings) return;

    const text = sayText.trim();
    const errors: FieldErrors = {};
    const length = charCount(text);
    if (length === 0) {
      errors.sayText = '播报内容不能为空';
    } else if (length > 500) {
      errors.sayText = `播报内容最多 500 字，当前 ${length} 字`;
    }
    if (targetMode === 'device' && !targetDevice) {
      errors.sayTarget = '请选择一台在线设备';
    }
    if (targetMode === 'room' && !targetRoom.trim()) {
      errors.sayTarget = '请填写目标房间';
    }
    setFieldErrors(errors);
    if (Object.keys(errors).length > 0) return;

    if (!mqttActive) {
      setSayError('MQTT 传输层未运行，主动播报不可用。请先切换协议并重启 haimen。');
      return;
    }

    setSending(true);
    setSayError(null);
    setSayResult(null);
    try {
      const result = await sayXiaozhi({
        text,
        device: targetMode === 'device' ? targetDevice : undefined,
        room: targetMode === 'room' ? targetRoom.trim() : undefined,
        token: savedSayToken,
      });
      setSayResult(result);
    } catch (error) {
      setSayError(describeSayError(error));
    } finally {
      setSending(false);
    }
  };

  const copyUrl = async (key: string, value: string) => {
    try {
      await navigator.clipboard.writeText(value);
      setCopiedKey(key);
      window.setTimeout(() => {
        setCopiedKey((current) => (current === key ? null : current));
      }, 1500);
    } catch {
      setCopiedKey(null);
    }
  };

  const onlineDevices = useMemo(() => devices.filter((device) => device.online), [devices]);
  const deviceOptions = useMemo(
    () =>
      onlineDevices.map((device) => ({ value: deviceValue(device), label: deviceLabel(device) })),
    [onlineDevices],
  );
  const roomOptions = useMemo(() => {
    const rooms = devices.map((device) => device.room.trim()).filter((room) => room.length > 0);
    return Array.from(new Set(rooms));
  }, [devices]);

  // 后端返回 mqtt_running 时以它为准；旧后端没有该字段时按协议推断
  const mqttActive = mqttRunning ?? settings?.protocol === 'mqtt';

  if (loading) {
    return (
      <div className="mx-auto max-w-4xl space-y-8 px-4 py-8">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">小智传输与播报</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            配置小智设备的传输协议、OTA 安全与主动播报。
          </p>
        </div>
        <Card>
          <CardHeader>
            <CardTitle className="text-base">加载配置</CardTitle>
          </CardHeader>
          <CardContent className="space-y-2">
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </CardContent>
        </Card>
      </div>
    );
  }

  if (loadError || !settings) {
    return (
      <div className="mx-auto max-w-4xl space-y-8 px-4 py-8">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">小智传输与播报</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            配置小智设备的传输协议、OTA 安全与主动播报。
          </p>
        </div>
        <Card>
          <CardContent className="pt-6">
            <Alert variant="destructive">
              <AlertTitle>加载失败</AlertTitle>
              <AlertDescription>{loadError ?? '配置为空'}</AlertDescription>
            </Alert>
            <Button variant="outline" className="mt-2" onClick={loadSettings}>
              重试
            </Button>
          </CardContent>
        </Card>
      </div>
    );
  }

  const origin = window.location.origin;
  const otaUrl = `${origin}/xiaozhi/ota/${settings.ota_token ? `?token=${encodeURIComponent(settings.ota_token)}` : ''}`;
  const wsUrl = `${origin.replace(/^http/, 'ws')}/xiaozhi/ws`;
  const mqtt = settings.mqtt;

  return (
    <div className="mx-auto max-w-4xl space-y-6 px-4 py-8">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">小智传输与播报</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          配置小智设备的传输协议、MQTT / UDP、OTA
          安全与主动播报。设备名称和房间请在「小智设备」页维护。
        </p>
      </div>

      {saveError && (
        <Alert variant="destructive">
          <AlertTitle>保存失败</AlertTitle>
          <AlertDescription>{saveError}</AlertDescription>
        </Alert>
      )}
      {saveResult && (
        <Alert className="border-green-500/50 bg-green-500/10">
          <AlertTitle className="text-green-700 dark:text-green-400">保存成功</AlertTitle>
          <AlertDescription className="text-green-700/90 dark:text-green-400/90">
            {saveResult}
          </AlertDescription>
        </Alert>
      )}
      {restartRequired && (
        <WarningAlert title="已保存，需重启 haimen 生效；设备需重新拉 OTA">
          传输层（协议 / MQTT /
          UDP）在进程启动时绑定，重启后才会监听新地址。重启完成后，设备需要重新拉取一次 OTA 配置。
        </WarningAlert>
      )}

      {/* 传输协议 */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">传输协议</CardTitle>
          <CardDescription>
            小智设备与 haimen 之间的通信方式。WebSocket 是默认单机模式；MQTT
            支持多设备、房间路由与主动播报。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <Field label="协议">
            <Combobox
              options={PROTOCOL_OPTIONS}
              value={settings.protocol}
              onChange={(value) =>
                patchSettings((prev) => ({
                  ...prev,
                  protocol: value === 'mqtt' ? 'mqtt' : 'websocket',
                }))
              }
              placeholder="选择传输协议"
            />
          </Field>
          {settings.protocol === 'mqtt' && (
            <WarningAlert title="切换到 MQTT 后需要重启">
              修改后需重启 haimen，设备需重新拉 OTA。重启前新 Broker 尚未监听，设备会保持旧连接。
            </WarningAlert>
          )}
        </CardContent>
      </Card>

      {/* MQTT */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">MQTT</CardTitle>
          <CardDescription>Broker 连接与设备主题模板；仅在协议为 MQTT 时使用。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="grid gap-4 sm:grid-cols-2">
            <Field
              label="Broker 地址"
              hint="host:port，例如 192.168.1.10:1883"
              error={fieldErrors.endpoint}
            >
              <Input
                value={mqtt.endpoint}
                placeholder="192.168.1.10:1883"
                className="font-mono"
                onChange={(e) => patchMqtt('endpoint', e.target.value)}
              />
            </Field>
            <Field label="Keepalive（秒）" error={fieldErrors.keepalive}>
              <Input
                type="number"
                min={1}
                value={Number.isFinite(mqtt.keepalive) ? String(mqtt.keepalive) : ''}
                placeholder="240"
                onChange={(e) =>
                  patchMqtt(
                    'keepalive',
                    e.target.value === '' ? Number.NaN : Number(e.target.value),
                  )
                }
              />
            </Field>
            <Field label="用户名">
              <Input
                value={mqtt.username}
                placeholder="MQTT 用户名（可选）"
                autoComplete="off"
                onChange={(e) => patchMqtt('username', e.target.value)}
              />
            </Field>
            <Field label="密码">
              <Input
                type="password"
                value={mqtt.password}
                placeholder="MQTT 密码（可选）"
                autoComplete="new-password"
                onChange={(e) => patchMqtt('password', e.target.value)}
              />
            </Field>
            <Field
              label="设备鉴权密钥（auth_key）"
              hint="设备接入时用于签名校验；留空 = 局域网内可注入/顶号"
            >
              <Input
                type="password"
                value={mqtt.auth_key}
                placeholder="建议设置为随机长字符串"
                autoComplete="new-password"
                onChange={(e) => patchMqtt('auth_key', e.target.value)}
              />
            </Field>
            <Field
              label="上行主题模板"
              hint="必须包含 {mac}，例如 haimen/xz/{mac}/up"
              error={fieldErrors.publishTopic}
            >
              <Input
                value={mqtt.publish_topic_template}
                className="font-mono"
                onChange={(e) => patchMqtt('publish_topic_template', e.target.value)}
              />
            </Field>
            <Field
              label="下行主题模板"
              hint="必须包含 {mac}，例如 haimen/xz/{mac}/down"
              error={fieldErrors.downlinkTopic}
            >
              <Input
                value={mqtt.downlink_topic_template}
                className="font-mono"
                onChange={(e) => patchMqtt('downlink_topic_template', e.target.value)}
              />
            </Field>
          </div>

          <SwitchRow
            label="鉴权宽限期（auth_grace）"
            description="开启后，设备首次接入鉴权失败时仍可在宽限窗口内连接，便于调试。"
            checked={mqtt.auth_grace}
            onCheckedChange={(checked) => patchMqtt('auth_grace', checked)}
          />

          {settings.protocol === 'mqtt' && mqtt.auth_key.trim() === '' && (
            <WarningAlert title="auth_key 为空：局域网内可注入/顶号">
              未设置设备鉴权密钥时，局域网内任何人都能伪造设备身份接入，或顶掉已在线设备。建议设置随机长字符串。
            </WarningAlert>
          )}
        </CardContent>
      </Card>

      {/* UDP */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">UDP</CardTitle>
          <CardDescription>UDP 传输 / 发现通道的监听地址；不使用时保持默认即可。</CardDescription>
        </CardHeader>
        <CardContent>
          <Field label="监听地址（bind_addr）" hint="host:port，例如 0.0.0.0:8888">
            <Input
              value={settings.udp.bind_addr}
              placeholder="0.0.0.0:8888"
              className="font-mono"
              onChange={(e) =>
                patchSettings((prev) => ({ ...prev, udp: { bind_addr: e.target.value } }))
              }
            />
          </Field>
        </CardContent>
      </Card>

      {/* OTA 安全 */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">OTA 安全</CardTitle>
          <CardDescription>
            限制哪些设备/来源可以拉取 OTA 配置。设备首次配网或重启后会自动请求 OTA 地址。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <SwitchRow
            label="仅允许内网设备拉取 OTA（ota_allow_private_only）"
            description="开启后，来自公网地址的 OTA 请求会被拒绝。"
            checked={settings.ota_allow_private_only}
            onCheckedChange={(checked) =>
              patchSettings((prev) => ({ ...prev, ota_allow_private_only: checked }))
            }
          />
          <Field label="OTA Token" hint="设置后 OTA 地址需要携带 ?token=...；留空表示不校验">
            <Input
              type="password"
              value={settings.ota_token}
              placeholder="建议设置为随机长字符串"
              autoComplete="new-password"
              onChange={(e) => patchSettings((prev) => ({ ...prev, ota_token: e.target.value }))}
            />
          </Field>
          <Field
            label="可信反向代理（ota_trusted_proxies）"
            hint="每行一个 IP（也可用逗号分隔）；仅当 haimen 位于反向代理之后时填写"
            error={fieldErrors.proxies}
          >
            <Textarea
              rows={3}
              value={proxiesText}
              placeholder={'127.0.0.1\n::1'}
              className="font-mono text-sm"
              onChange={(e) => {
                setProxiesText(e.target.value);
                markDirty();
              }}
            />
          </Field>
          {(settings.ota_token.trim() === '' || !settings.ota_allow_private_only) && (
            <WarningAlert title="OTA 防护不完整">
              {settings.ota_token.trim() === ''
                ? '未设置 OTA Token，任何能访问本机端口的设备都可以拉取配置。'
                : '未开启「仅允许内网设备」，公网来源也可能拉取 OTA 配置。'}
              建议同时设置 OTA Token 并开启来源限制。
            </WarningAlert>
          )}
        </CardContent>
      </Card>

      {/* 主动播报 */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="flex flex-wrap items-center gap-2 text-base">
            主动播报
            {mqttRunning === true && (
              <Badge
                variant="outline"
                className="border-transparent bg-green-500/15 text-green-600"
              >
                MQTT 运行中
              </Badge>
            )}
            {mqttRunning === false && (
              <Badge variant="outline" className="text-muted-foreground">
                MQTT 未运行
              </Badge>
            )}
          </CardTitle>
          <CardDescription>
            调用 TTS 合成语音并下发给在线设备。只支持 MQTT 模式，设备需处于空闲状态。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <Field
            label="调用 Token（say_token）"
            hint="用于 /api/v1/xiaozhi/say 与设备清单鉴权；留空表示不校验。测试使用已保存的 Token，修改后请先保存。"
          >
            <Input
              type="password"
              value={settings.say_token}
              placeholder="建议设置为随机长字符串"
              autoComplete="new-password"
              onChange={(e) => patchSettings((prev) => ({ ...prev, say_token: e.target.value }))}
            />
          </Field>

          {!mqttActive && (
            <WarningAlert title="当前 MQTT 传输层未运行">
              主动播报需要 MQTT 模式。请把「传输协议」改为 MQTT，保存后重启
              haimen；设备还需要重新拉取 OTA 才会连上 Broker。
            </WarningAlert>
          )}

          <div className="flex flex-wrap items-center gap-2">
            <span className="text-sm font-medium">在线设备</span>
            <span className="text-xs text-muted-foreground">
              {onlineDevices.length} 台在线 / 共 {devices.length} 台
            </span>
            <Button
              variant="outline"
              size="sm"
              disabled={devicesLoading}
              onClick={() => loadDevices(savedSayToken)}
            >
              {devicesLoading ? '刷新中...' : '刷新设备'}
            </Button>
          </div>

          {devicesError && (
            <Alert variant="destructive">
              <AlertTitle>设备清单获取失败</AlertTitle>
              <AlertDescription>{devicesError}</AlertDescription>
            </Alert>
          )}

          <Field label="目标" error={fieldErrors.sayTarget}>
            <Combobox
              options={TARGET_MODE_OPTIONS}
              value={targetMode}
              onChange={(value) => {
                setTargetMode(value === 'device' || value === 'room' ? value : 'auto');
                setSayError(null);
                setSayResult(null);
                setFieldErrors((prev) => ({ ...prev, sayTarget: undefined }));
              }}
              placeholder="选择目标方式"
            />
          </Field>

          {targetMode === 'device' && (
            <Field
              label="目标设备"
              hint={
                onlineDevices.length === 0
                  ? '没有在线设备。请确认 MQTT 已启动、设备已上电，然后点「刷新设备」。'
                  : '只列出在线设备；显示格式为 名称 · 房间 · MAC。'
              }
            >
              <Combobox
                options={deviceOptions}
                value={targetDevice || null}
                onChange={(value) => {
                  setTargetDevice(value);
                  setSayError(null);
                  setSayResult(null);
                  setFieldErrors((prev) => ({ ...prev, sayTarget: undefined }));
                }}
                placeholder={onlineDevices.length === 0 ? '暂无在线设备' : '选择设备'}
                searchPlaceholder="按名称 / 房间 / MAC 搜索..."
                emptyText="没有匹配的在线设备"
              />
            </Field>
          )}

          {targetMode === 'room' && (
            <Field label="目标房间" hint="从已配置设备的房间中选择，或直接输入房间名">
              <Input
                list="xiaozhi-room-options"
                value={targetRoom}
                placeholder="如：客厅"
                onChange={(e) => {
                  setTargetRoom(e.target.value);
                  setSayError(null);
                  setSayResult(null);
                  setFieldErrors((prev) => ({ ...prev, sayTarget: undefined }));
                }}
              />
              <datalist id="xiaozhi-room-options">
                {roomOptions.map((room) => (
                  <option key={room} value={room} />
                ))}
              </datalist>
            </Field>
          )}

          <Field
            label="播报内容"
            hint={`最多 500 字（当前 ${charCount(sayText)} 字）`}
            error={fieldErrors.sayText}
          >
            <Textarea
              rows={3}
              value={sayText}
              placeholder="例如：客厅的灯已经关好了，晚安。"
              onChange={(e) => {
                setSayText(e.target.value);
                setSayResult(null);
                setSayError(null);
                setFieldErrors((prev) => ({ ...prev, sayText: undefined }));
              }}
            />
          </Field>

          <div className="flex flex-wrap items-center gap-2">
            <Button
              onClick={handleSay}
              disabled={sending || !mqttActive}
              title={mqttActive ? undefined : 'MQTT 传输层未运行'}
            >
              {sending ? '发送中...' : '发送测试'}
            </Button>
            <span className="text-xs text-muted-foreground">
              成功只代表已下发，不保证设备一定播放（notify 无 ACK）。
            </span>
          </div>

          {sayResult && (
            <Alert className="border-green-500/50 bg-green-500/10">
              <AlertTitle className="text-green-700 dark:text-green-400">
                已下发到「{sayResult.name || sayResult.device}」
                {sayResult.room ? `（${sayResult.room}）` : ''}
              </AlertTitle>
              <AlertDescription className="space-y-1 text-green-700/90 dark:text-green-400/90">
                <p>
                  模式：{sayResult.mode} · 时长：{formatDuration(sayResult.duration_ms)}
                </p>
                <p className="break-all">
                  音频地址：
                  <a
                    href={sayResult.audio_url}
                    target="_blank"
                    rel="noreferrer"
                    className="underline underline-offset-2"
                  >
                    {sayResult.audio_url}
                  </a>
                </p>
              </AlertDescription>
            </Alert>
          )}

          {sayError && (
            <Alert variant="destructive">
              <AlertTitle>下发失败</AlertTitle>
              <AlertDescription>{sayError}</AlertDescription>
            </Alert>
          )}
        </CardContent>
      </Card>

      {/* 保存 */}
      <div className="flex flex-wrap items-center gap-3">
        <Button onClick={handleSave} disabled={saving}>
          {saving ? '保存中...' : '保存配置'}
        </Button>
        <span className="text-xs text-muted-foreground">
          保存对传输协议、MQTT、UDP、OTA 与 say_token 一并生效；传输层改动需重启 haimen。
        </span>
      </div>

      {/* 接入地址 */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">接入地址</CardTitle>
          <CardDescription>
            在设备配网 / OTA 配置里填写这两个地址。局域网设备必须使用 haimen 所在机器的局域网 IP。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <Field
            label="OTA 地址"
            hint={
              settings.ota_token
                ? '已根据 ota_token 自动拼接 ?token=...，复制后可直接使用。'
                : '设置 ota_token 后会自动拼接 ?token=...。'
            }
          >
            <div className="flex gap-2">
              <Input readOnly value={otaUrl} className="font-mono text-xs" />
              <Button variant="outline" onClick={() => copyUrl('ota', otaUrl)}>
                {copiedKey === 'ota' ? '已复制' : '复制'}
              </Button>
            </div>
          </Field>
          <Field label="WebSocket 地址" hint="WebSocket 模式下设备直连的地址。">
            <div className="flex gap-2">
              <Input readOnly value={wsUrl} className="font-mono text-xs" />
              <Button variant="outline" onClick={() => copyUrl('ws', wsUrl)}>
                {copiedKey === 'ws' ? '已复制' : '复制'}
              </Button>
            </div>
          </Field>
          <Alert>
            <AlertTitle>不要把 localhost 填给设备</AlertTitle>
            <AlertDescription>
              当前页面从 {origin} 打开，但小智设备在另一台机器上，localhost / 127.0.0.1
              指向设备自己。请把地址中的主机部分替换成 haimen 所在机器的局域网 IP（如
              192.168.1.10）。MQTT 模式下设备通过 Broker 通信，OTA 地址仍用于下发传输配置。
            </AlertDescription>
          </Alert>
        </CardContent>
      </Card>
    </div>
  );
}
