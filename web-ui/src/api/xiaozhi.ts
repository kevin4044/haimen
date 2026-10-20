import type {
  ApiResponse,
  XiaozhiConfiguredDevice,
  XiaozhiDeviceState,
  XiaozhiDevicesData,
  XiaozhiSayResult,
  XiaozhiSettings,
} from '@/types';
import { apiFetch } from './client';

function ensureData<T>(res: ApiResponse<T>): T {
  if (res.data == null) throw new Error('Empty response');
  return res.data;
}

/**
 * 控制面设备条目（`GET /api/v1/xiaozhi/devices`）。
 *
 * 与设置页的设备目录不同，这个接口会同时返回已配置但离线的设备：
 * 离线条目没有 `client_id`，`state` 也是 `null`。前端据此区分
 * 「设备离线」和「MQTT 传输层没跑」。
 */
export interface XiaozhiControlDevice {
  client_id?: string | null;
  device_id: string;
  name: string;
  room: string;
  /** 错误响应（如 multiple_devices）里的候选设备可能不带在线状态 */
  online?: boolean;
  state?: XiaozhiDeviceState | null;
}

/** `GET /api/v1/xiaozhi/devices` 的结果 */
export interface XiaozhiControlDevicesResult {
  devices: XiaozhiControlDevice[];
  /**
   * MQTT 传输层是否真的在跑。旧版后端可能没有这个字段，
   * 此时为 `null`，由调用方按 `settings.protocol === 'mqtt'` 推断。
   */
  mqttRunning: boolean | null;
}

/**
 * say / devices 控制面的错误。
 *
 * 与设置接口的 `ApiError` 分开：控制面返回 `{ok:false,error,...}`，
 * 错误码（`device_offline` / `device_busy` / ...）需要被页面直接识别。
 */
export class XiaozhiError extends Error {
  readonly code: string;
  readonly status: number;
  readonly detail?: string;
  readonly devices?: XiaozhiControlDevice[];
  readonly state?: string;
  readonly room?: string;

  constructor(
    code: string,
    status: number,
    message: string,
    extra: {
      detail?: string;
      devices?: XiaozhiControlDevice[];
      state?: string;
      room?: string;
    } = {},
  ) {
    super(message);
    this.name = 'XiaozhiError';
    this.code = code;
    this.status = status;
    this.detail = extra.detail;
    this.devices = extra.devices;
    this.state = extra.state;
    this.room = extra.room;
  }
}

/** 保存设备表时提交的单台设备（device_id 支持 MAC 或 MQTT client_id） */
export interface XiaozhiDeviceInput {
  device_id: string;
  name: string;
  room: string;
}

/** 仅当 token 非空时追加 Bearer 头，空串代表后端不校验 */
function authHeaders(token?: string): Record<string, string> {
  const value = token?.trim();
  return value ? { Authorization: `Bearer ${value}` } : {};
}

/** 设置接口沿用 `{success,data}` 信封，控制面则是 `{ok,...}`，两者不能混用 */
interface XiaozhiControlErrorBody {
  ok?: false;
  error?: string;
  detail?: string;
  state?: string;
  room?: string;
  devices?: XiaozhiControlDevice[];
}

interface XiaozhiControlDevicesBody {
  ok?: true;
  mqtt_running?: boolean;
  devices?: XiaozhiControlDevice[];
}

/** 把控制面的失败响应转成 `XiaozhiError`（网络错误仍由 fetch 直接抛出） */
async function toXiaozhiError(res: Response): Promise<XiaozhiError> {
  let body: XiaozhiControlErrorBody = {};
  try {
    body = (await res.json()) as XiaozhiControlErrorBody;
  } catch {
    // 非 JSON 错误页（代理 / 网关）：下面用状态码兜底
  }
  const code = typeof body.error === 'string' && body.error ? body.error : `http_${res.status}`;
  const message = body.detail || body.error || res.statusText || '请求失败';
  return new XiaozhiError(code, res.status, message, {
    detail: body.detail,
    devices: body.devices,
    state: body.state,
    room: body.room,
  });
}

/**
 * 获取已配置设备与当前在线设备。
 *
 * 这是**设置页**的设备目录（`/settings/xiaozhi/devices`）。`token` 仅在
 * 调用方显式传入时用于 Bearer 鉴权，不传时行为与之前完全一致。
 */
export async function getXiaozhiDevices(token?: string): Promise<XiaozhiDevicesData> {
  const res = await apiFetch<ApiResponse<XiaozhiDevicesData>>('/api/v1/settings/xiaozhi/devices', {
    headers: authHeaders(token),
  });
  return ensureData(res);
}

/** 全量替换设备表（保存后后端立即生效，无需重启） */
export async function updateXiaozhiDevices(
  devices: XiaozhiDeviceInput[],
): Promise<XiaozhiConfiguredDevice[]> {
  const res = await apiFetch<ApiResponse<{ devices: XiaozhiConfiguredDevice[] }>>(
    '/api/v1/settings/xiaozhi/devices',
    {
      method: 'PUT',
      body: JSON.stringify({ devices }),
    },
  );
  return ensureData(res).devices;
}

/**
 * 读取传输 / OTA / 主动播报配置。
 *
 * 后端返回 `{success:true,data:{...}}`，这里剥掉信封只给页面配置对象。
 */
export async function getXiaozhiSettings(): Promise<XiaozhiSettings> {
  const res = await apiFetch<ApiResponse<XiaozhiSettings>>('/api/v1/settings/xiaozhi');
  return ensureData(res);
}

/** `PUT /api/v1/settings/xiaozhi` 的结果（部分更新，页面可提交完整对象） */
export interface XiaozhiSettingsUpdateResult {
  settings: XiaozhiSettings;
  /** 传输层（protocol/mqtt/udp）有变化，需要重启 haimen 才生效 */
  restartRequired: boolean;
  message: string;
}

export async function updateXiaozhiSettings(
  settings: Partial<XiaozhiSettings>,
): Promise<XiaozhiSettingsUpdateResult> {
  const res = await apiFetch<ApiResponse<XiaozhiSettings>>('/api/v1/settings/xiaozhi', {
    method: 'PUT',
    body: JSON.stringify(settings),
  });
  return {
    settings: ensureData(res),
    restartRequired: res.restart_required ?? false,
    message: res.message ?? '',
  };
}

/**
 * 控制面设备清单（`GET /api/v1/xiaozhi/devices`）。
 *
 * 响应是 `{ok:true,mqtt_running,devices}`，不是设置接口的 `{success,data}`，
 * 所以这里直接走 `fetch`，不复用 `apiFetch`。
 */
export async function getXiaozhiControlDevices(
  token?: string,
): Promise<XiaozhiControlDevicesResult> {
  const res = await fetch('/api/v1/xiaozhi/devices', {
    headers: authHeaders(token),
  });
  if (!res.ok) throw await toXiaozhiError(res);

  const body = (await res.json()) as XiaozhiControlDevicesBody;
  return {
    devices: Array.isArray(body.devices) ? body.devices : [],
    mqttRunning: typeof body.mqtt_running === 'boolean' ? body.mqtt_running : null,
  };
}

export interface XiaozhiSayInput {
  text: string;
  /** 目标设备：MAC（任意写法）、MQTT client_id 或配置的设备名 */
  device?: string;
  /** 目标房间：设备目录里配置的 room */
  room?: string;
  /** 主动播报鉴权 token（后端 say_token 非空时必填） */
  token?: string;
}

/**
 * 让一台在线小智设备朗读文本（`POST /api/v1/xiaozhi/say`）。
 *
 * 失败时抛出 `XiaozhiError`，页面按 `code` 给出可操作的中文提示。
 * 注意 200 只表示「已下发」，不保证设备真的播放（notify 没有 ACK）。
 */
export async function sayXiaozhi({
  text,
  device,
  room,
  token,
}: XiaozhiSayInput): Promise<XiaozhiSayResult> {
  const res = await fetch('/api/v1/xiaozhi/say', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(token),
    },
    body: JSON.stringify({ text, device, room }),
  });
  if (!res.ok) throw await toXiaozhiError(res);
  return (await res.json()) as XiaozhiSayResult;
}
