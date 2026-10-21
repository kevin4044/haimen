/**
 * 小智自定义主题 assets 接口。
 *
 * 与设置接口的 `{success,data}` 信封不同，assets 控制面统一返回 `{ok,...}`，
 * 失败时是 `{ok:false,error,detail}`。这里不复用 `apiFetch`，直接走 fetch
 * 并把错误码包装成 `XiaozhiAssetsError`，方便页面按 `code` 给出中文提示。
 */

/** `GET /api/v1/xiaozhi/assets/device-info` 里的屏幕信息 */
export interface XiaozhiAssetsScreen {
  width: number;
  height: number;
  monochrome: boolean;
}

/**
 * 设备信息（屏幕分辨率 / assets 分区 / 在线状态）。
 *
 * `screen` 与 `assets_partition_size` 在设备未上报时为 `null`；
 * `mcp` 是设备 MCP 控制面能力（形状由固件决定），这里保持 unknown。
 */
export interface XiaozhiAssetsDeviceInfo {
  ok: true;
  device: string;
  online: boolean;
  client_id: string | null;
  mcp: unknown;
  screen: XiaozhiAssetsScreen | null;
  assets_partition_size: number | null;
  warning: string | null;
}

/** `POST /api/v1/xiaozhi/assets` 成功响应 */
export interface XiaozhiAssetsUploadResult {
  ok: true;
  file: string;
  size: number;
  sha256: string;
  url: string;
}

/** `POST /api/v1/xiaozhi/assets/apply` 成功响应 */
export interface XiaozhiAssetsApplyResult {
  ok: true;
  device: string;
  file: string;
  url: string;
  reboot: boolean;
}

/**
 * assets 接口错误。
 *
 * `code` 对应后端 `{ok:false,error}`；HTTP 层失败（代理 / 网关）时回退
 * `http_<status>`，页面可以据此区分校验失败和设备离线等场景。
 */
export class XiaozhiAssetsError extends Error {
  readonly code: string;
  readonly status: number;
  readonly detail?: string;

  constructor(code: string, status: number, message: string, detail?: string) {
    super(message);
    this.name = 'XiaozhiAssetsError';
    this.code = code;
    this.status = status;
    this.detail = detail;
  }
}

interface AssetsErrorBody {
  ok?: false;
  error?: string;
  detail?: string;
}

/** 把失败响应转成 `XiaozhiAssetsError`（网络错误仍由 fetch 直接抛出） */
async function toAssetsError(res: Response): Promise<XiaozhiAssetsError> {
  let body: AssetsErrorBody = {};
  try {
    body = (await res.json()) as AssetsErrorBody;
  } catch {
    // 非 JSON 错误页（网关 / 代理）：下面用状态码兜底
  }
  const detail = typeof body.detail === 'string' && body.detail ? body.detail : undefined;
  const code = typeof body.error === 'string' && body.error ? body.error : `http_${res.status}`;
  const message = detail || (typeof body.error === 'string' && body.error) || res.statusText;
  return new XiaozhiAssetsError(code, res.status, message || '请求失败', detail);
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

/** 宽松解析屏幕信息：宽高必须是正数，否则视为未上报 */
function parseScreen(value: unknown): XiaozhiAssetsScreen | null {
  const screen = asRecord(value);
  const width = screen.width;
  const height = screen.height;
  if (typeof width !== 'number' || typeof height !== 'number') return null;
  if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0) return null;
  return { width, height, monochrome: Boolean(screen.monochrome) };
}

/** 读取设备信息：分辨率、assets 分区大小、在线 / MCP 状态 */
export async function getXiaozhiAssetsDeviceInfo(
  device: string,
  signal?: AbortSignal,
): Promise<XiaozhiAssetsDeviceInfo> {
  const res = await fetch(
    `/api/v1/xiaozhi/assets/device-info?device=${encodeURIComponent(device)}`,
    {
      signal,
    },
  );
  if (!res.ok) throw await toAssetsError(res);

  const body = asRecord(await res.json().catch(() => ({})));
  if (body.ok === false) {
    const code = typeof body.error === 'string' && body.error ? body.error : 'device_info_failed';
    const detail = typeof body.detail === 'string' && body.detail ? body.detail : undefined;
    throw new XiaozhiAssetsError(code, res.status, detail ?? '读取设备信息失败', detail);
  }
  const partition = body.assets_partition_size;
  return {
    ok: true,
    device: typeof body.device === 'string' && body.device ? body.device : device,
    online: Boolean(body.online),
    client_id: typeof body.client_id === 'string' ? body.client_id : null,
    mcp: body.mcp,
    screen: parseScreen(body.screen),
    assets_partition_size:
      typeof partition === 'number' && Number.isFinite(partition) && partition > 0
        ? partition
        : null,
    warning: typeof body.warning === 'string' && body.warning ? body.warning : null,
  };
}

/**
 * 上传打包好的 assets.bin（`POST /api/v1/xiaozhi/assets`）。
 *
 * 请求体是裸二进制，文件名通过 `X-File-Name` 头传递；成功后返回后端
 * 落盘的文件名与下载地址，供 apply 步骤引用。
 */
export async function uploadXiaozhiAssets(
  data: Uint8Array,
  fileName = 'assets.bin',
): Promise<XiaozhiAssetsUploadResult> {
  // 复制成独立 ArrayBuffer，避免 TS 把 Uint8Array<ArrayBufferLike> 判为不可作 BodyInit
  const payload = new Uint8Array(data);
  const res = await fetch('/api/v1/xiaozhi/assets', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/octet-stream',
      'X-File-Name': fileName,
    },
    body: payload.buffer,
  });
  if (!res.ok) throw await toAssetsError(res);

  const body = asRecord(await res.json().catch(() => ({})));
  const file = typeof body.file === 'string' ? body.file : '';
  if (!file) {
    throw new XiaozhiAssetsError('upload_failed', res.status, '上传响应缺少文件名');
  }
  return {
    ok: true,
    file,
    size: typeof body.size === 'number' ? body.size : data.byteLength,
    sha256: typeof body.sha256 === 'string' ? body.sha256 : '',
    url: typeof body.url === 'string' ? body.url : '',
  };
}

/**
 * 把已上传的 assets 应用到设备（`POST /api/v1/xiaozhi/assets/apply`）。
 *
 * 后端通过 MCP 调 `self.assets.set_download_url` 并下发重启；成功只代表
 * 指令已发出，设备真正重启换主题需要等待几秒。
 */
export async function applyXiaozhiAssets(
  device: string,
  file: string,
): Promise<XiaozhiAssetsApplyResult> {
  const res = await fetch('/api/v1/xiaozhi/assets/apply', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ device, file }),
  });
  if (!res.ok) throw await toAssetsError(res);

  const body = asRecord(await res.json().catch(() => ({})));
  return {
    ok: true,
    device: typeof body.device === 'string' ? body.device : device,
    file: typeof body.file === 'string' ? body.file : file,
    url: typeof body.url === 'string' ? body.url : '',
    reboot: Boolean(body.reboot),
  };
}
