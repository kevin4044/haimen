import { XiaozhiAssetsError } from '@/api/xiaozhiAssets';

/**
 * 小智自定义主题 assets 工具。
 *
 * 包含三类纯逻辑：
 * 1. `assets.bin` 打包：12 字节头 + 44 字节文件表 + 0x5A5A 前缀数据，
 *    与官方 `spiffs_assets_gen.py` / `xiaozhi-assets-generator` 一致；
 * 2. LVGL 背景图：28 字节 `lv_image_dsc_t` 头 + RGB565 LE 像素；
 * 3. 浏览器端图片处理：Canvas cover 裁剪、表情图缩放。
 */

/** 固件支持的 21 个固定表情槽（顺序与官方 emoji_collection 一致） */
export const EMOJI_NAMES = [
  'neutral',
  'happy',
  'laughing',
  'funny',
  'sad',
  'angry',
  'crying',
  'loving',
  'embarrassed',
  'surprised',
  'shocked',
  'thinking',
  'winking',
  'cool',
  'relaxed',
  'delicious',
  'kissy',
  'confident',
  'sleepy',
  'silly',
  'confused',
] as const;

export type EmojiName = (typeof EMOJI_NAMES)[number];

/** 表情文件允许的上传格式（小写扩展名） */
export const EMOJI_EXTENSIONS = ['png', 'gif', 'jpg', 'jpeg', 'webp'] as const;

/** 静态表情最大边长（GIF 原样上传，不走缩放） */
export const MAX_EMOJI_SIZE = 64;

/** assets.bin 文件头长度：total_files(4) + checksum(4) + combined_len(4) */
export const ASSETS_HEADER_SIZE = 12;

/** 文件表里文件名的固定长度 */
export const ASSETS_NAME_LENGTH = 32;

/** 单个文件表项长度：name(32) + size(4) + offset(4) + width(2) + height(2) */
export const ASSETS_TABLE_ENTRY_SIZE = ASSETS_NAME_LENGTH + 12;

/** 每个文件数据前的 0x5A5A 魔数 */
export const ASSETS_DATA_MAGIC = 0x5a5a;

/** LVGL v9 `lv_image_dsc_t` 头部长度（官方生成器固定写 28 字节） */
export const LVGL_IMAGE_HEADER_SIZE = 28;

/** 浅色 / 深色模式默认背景色（官方主题默认值） */
export const DEFAULT_LIGHT_BACKGROUND = '#ffffff';
export const DEFAULT_DARK_BACKGROUND = '#121212';

/** 打包进 assets.bin 的单个文件 */
export interface AssetsFile {
  name: string;
  data: Uint8Array;
  /** 图片类资源的原始宽高，写入文件表；非图片为 0 */
  width?: number;
  height?: number;
}

/** 主题的深 / 浅色皮肤配置（对应 index.json 的 skin.light / skin.dark） */
export interface AssetsSkinMode {
  text_color: string;
  background_color: string;
  /** 背景图片文件名（仅图片模式），如 background_light.raw */
  background_image?: string;
}

/** 生成的 index.json（固件据此应用主题） */
export interface AssetsIndexJson {
  version: number;
  hide_subtitle: boolean;
  display_config: {
    width: number;
    height: number;
    monochrome: boolean;
    color: 'RGB565';
  };
  skin?: {
    light?: AssetsSkinMode;
    dark?: AssetsSkinMode;
  };
  emoji_collection?: { name: EmojiName; file: string }[];
  /** 自定义字体文件名；默认不写，保持设备当前字体 */
  text_font?: string;
}

export interface BuildIndexOptions {
  screen: { width: number; height: number; monochrome?: boolean };
  light: {
    mode: 'solid' | 'image';
    color: string;
    /** 图片模式下是否已生成 background_light.raw */
    hasImage: boolean;
  };
  dark: {
    mode: 'solid' | 'image';
    color: string;
    hasImage: boolean;
  };
  hideSubtitle: boolean;
  emojiCollection?: { name: EmojiName; file: string }[];
  /** 自定义字体文件名（如 font_custom.bin）；为空表示保持当前字体 */
  textFont?: string;
}

/** 默认文字颜色：浅色模式黑字、深色模式白字（与官方模板一致） */
const LIGHT_TEXT_COLOR = '#000000';
const DARK_TEXT_COLOR = '#ffffff';

/** 生成 index.json 对象；未启用的可选项不会写入 */
export function buildIndexJson(options: BuildIndexOptions): AssetsIndexJson {
  const index: AssetsIndexJson = {
    version: 1,
    hide_subtitle: options.hideSubtitle,
    display_config: {
      width: options.screen.width,
      height: options.screen.height,
      monochrome: Boolean(options.screen.monochrome),
      color: 'RGB565',
    },
  };

  const light: AssetsSkinMode = {
    text_color: LIGHT_TEXT_COLOR,
    background_color: options.light.color,
  };
  if (options.light.mode === 'image' && options.light.hasImage) {
    light.background_image = 'background_light.raw';
  }
  const dark: AssetsSkinMode = {
    text_color: DARK_TEXT_COLOR,
    background_color: options.dark.color,
  };
  if (options.dark.mode === 'image' && options.dark.hasImage) {
    dark.background_image = 'background_dark.raw';
  }
  index.skin = { light, dark };

  const collection = (options.emojiCollection ?? []).filter((item) => item.file);
  if (collection.length > 0) index.emoji_collection = collection;
  if (options.textFont) index.text_font = options.textFont;

  return index;
}

/** 小端写入固定长度文件名：UTF-8 编码，超长截断，剩余补 0 */
function writeFixedName(target: Uint8Array, offset: number, name: string, length: number): void {
  const encoded = new TextEncoder().encode(name);
  target.set(encoded.subarray(0, length), offset);
}

/**
 * 打包 assets.bin。
 *
 * 布局（全部小端）：
 * - 头 12 字节：total_files u32、checksum u32（表 + 数据逐字节求和 & 0xffff）、combined_len u32；
 * - 文件表：每项 44 字节 name[32] + size u32 + offset u32 + width u16 + height u16，
 *   offset 相对数据区起点，指向该文件的 0x5A5A 前缀；
 * - 数据区：每个文件前加 0x5A 0x5A，再接原始字节；
 * - `index.json` 始终由 `index` 生成并排在最前，调用方无需自行加入。
 *
 * checksum 覆盖「文件表 + 数据区」，不包含 12 字节头。
 */
export function buildAssetsBin(entries: readonly AssetsFile[], index: object): Uint8Array {
  const indexBytes = new TextEncoder().encode(JSON.stringify(index, null, 2));
  const files: Required<AssetsFile>[] = [
    { name: 'index.json', data: indexBytes, width: 0, height: 0 },
    ...entries
      .filter((entry) => entry.name !== 'index.json')
      .map((entry) => ({
        name: entry.name,
        data: entry.data,
        width: entry.width ?? 0,
        height: entry.height ?? 0,
      })),
  ];

  const tableSize = files.length * ASSETS_TABLE_ENTRY_SIZE;
  const dataSize = files.reduce((total, file) => total + 2 + file.data.byteLength, 0);
  const combined = new Uint8Array(tableSize + dataSize);
  const view = new DataView(combined.buffer);

  let tableOffset = 0;
  let dataOffset = tableSize;
  for (const file of files) {
    writeFixedName(combined, tableOffset, file.name, ASSETS_NAME_LENGTH);
    tableOffset += ASSETS_NAME_LENGTH;
    view.setUint32(tableOffset, file.data.byteLength, true);
    tableOffset += 4;
    // offset 相对数据区起点，指向 0x5A5A 前缀
    view.setUint32(tableOffset, dataOffset - tableSize, true);
    tableOffset += 4;
    view.setUint16(tableOffset, file.width & 0xffff, true);
    tableOffset += 2;
    view.setUint16(tableOffset, file.height & 0xffff, true);
    tableOffset += 2;

    combined[dataOffset] = (ASSETS_DATA_MAGIC >> 8) & 0xff;
    combined[dataOffset + 1] = ASSETS_DATA_MAGIC & 0xff;
    dataOffset += 2;
    combined.set(file.data, dataOffset);
    dataOffset += file.data.byteLength;
  }

  let checksum = 0;
  for (const byte of combined) checksum += byte;

  const result = new Uint8Array(ASSETS_HEADER_SIZE + combined.byteLength);
  const resultView = new DataView(result.buffer);
  resultView.setUint32(0, files.length, true);
  resultView.setUint32(4, checksum & 0xffff, true);
  resultView.setUint32(8, combined.byteLength, true);
  result.set(combined, ASSETS_HEADER_SIZE);
  return result;
}

/**
 * 构造 28 字节 LVGL 背景图头（官方生成器的固定布局）：
 * magic(0x19) | cf(0x12) | flags u16、w u16 | h u16、stride u16 | reserved u16、
 * data_size u32、data 指针占位 u32(28)、reserved u32、reserved_2 u32。
 */
export function buildLvglImageHeader(width: number, height: number, dataSize: number): Uint8Array {
  const header = new Uint8Array(LVGL_IMAGE_HEADER_SIZE);
  const view = new DataView(header.buffer);
  header[0] = 0x19;
  header[1] = 0x12;
  view.setUint16(2, 0, true);
  view.setUint16(4, width & 0xffff, true);
  view.setUint16(6, height & 0xffff, true);
  view.setUint16(8, (width * 2) & 0xffff, true);
  view.setUint16(10, 0, true);
  view.setUint32(12, dataSize, true);
  view.setUint32(16, LVGL_IMAGE_HEADER_SIZE, true);
  view.setUint32(20, 0, true);
  view.setUint32(24, 0, true);
  return header;
}

/** 把 RGB565 像素数据包成「28 字节头 + 像素」的 background_*.raw */
export function wrapBackgroundRaw(rgb565: Uint8Array, width: number, height: number): Uint8Array {
  const result = new Uint8Array(LVGL_IMAGE_HEADER_SIZE + rgb565.byteLength);
  result.set(buildLvglImageHeader(width, height, rgb565.byteLength), 0);
  result.set(rgb565, LVGL_IMAGE_HEADER_SIZE);
  return result;
}

/** RGBA 像素转 RGB565（小端字节序，每像素 2 字节） */
export function rgbaToRgb565(rgba: Uint8ClampedArray | Uint8Array): Uint8Array {
  const pixelCount = Math.floor(rgba.length / 4);
  const result = new Uint8Array(pixelCount * 2);
  for (let i = 0; i < pixelCount; i += 1) {
    const r = rgba[i * 4] >> 3;
    const g = rgba[i * 4 + 1] >> 2;
    const b = rgba[i * 4 + 2] >> 3;
    const value = (r << 11) | (g << 5) | b;
    result[i * 2] = value & 0xff;
    result[i * 2 + 1] = (value >> 8) & 0xff;
  }
  return result;
}

/** cover 裁剪参数：等比放大到铺满目标尺寸并居中，超出部分裁掉 */
export interface CoverFitRect {
  offsetX: number;
  offsetY: number;
  drawWidth: number;
  drawHeight: number;
}

export function coverFitRect(
  imageWidth: number,
  imageHeight: number,
  targetWidth: number,
  targetHeight: number,
): CoverFitRect {
  if (
    !Number.isFinite(imageWidth) ||
    !Number.isFinite(imageHeight) ||
    !Number.isFinite(targetWidth) ||
    !Number.isFinite(targetHeight) ||
    imageWidth <= 0 ||
    imageHeight <= 0 ||
    targetWidth <= 0 ||
    targetHeight <= 0
  ) {
    return { offsetX: 0, offsetY: 0, drawWidth: targetWidth, drawHeight: targetHeight };
  }
  const scale = Math.max(targetWidth / imageWidth, targetHeight / imageHeight);
  const drawWidth = imageWidth * scale;
  const drawHeight = imageHeight * scale;
  return {
    offsetX: (targetWidth - drawWidth) / 2,
    offsetY: (targetHeight - drawHeight) / 2,
    drawWidth,
    drawHeight,
  };
}

/** 字节数展示：B / KB / MB */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '未知大小';
  if (bytes < 1024) return `${bytes} B`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb.toFixed(kb >= 100 ? 0 : 1)} KB`;
  return `${(kb / 1024).toFixed(2)} MB`;
}

/** 从文件名取小写扩展名（不含点） */
export function fileExtension(fileName: string): string {
  const parts = fileName.toLowerCase().split('.');
  return parts.length > 1 ? (parts.at(-1) ?? '') : '';
}

/** 判断上传文件是否为受支持的表情图片 */
export function isSupportedEmojiFile(file: File): boolean {
  const ext = fileExtension(file.name);
  return (EMOJI_EXTENSIONS as readonly string[]).includes(ext);
}

/** 判断文件是否为 GIF（按 MIME 或扩展名） */
export function isGifFile(file: File): boolean {
  return file.type === 'image/gif' || fileExtension(file.name) === 'gif';
}

/** 读取文件全部字节 */
export async function readFileBytes(file: File | Blob): Promise<Uint8Array> {
  return new Uint8Array(await file.arrayBuffer());
}

/** 加载图片为 HTMLImageElement（自动回收 object URL） */
export function loadImageElement(file: File | Blob): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const image = new Image();
    image.onload = () => {
      URL.revokeObjectURL(url);
      resolve(image);
    };
    image.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error('无法解析图片，请换一张 PNG/JPG/GIF/WEBP 图片'));
    };
    image.src = url;
  });
}

/** Canvas 转 Blob；`toBlob` 失败（尺寸过大/浏览器限制）时抛错 */
function canvasToBlob(canvas: HTMLCanvasElement, mime: string): Promise<Blob> {
  return new Promise((resolve, reject) => {
    canvas.toBlob(
      (blob) => {
        if (blob) resolve(blob);
        else reject(new Error('图片转码失败，请换一张更小的图片'));
      },
      mime,
      0.92,
    );
  });
}

/**
 * 把背景图片转换成 background_*.raw（cover 铺满 screen 分辨率 + LVGL 头）。
 * 只应在浏览器环境调用；设备分辨率未知时调用方应先提示。
 */
export async function imageToBackgroundRaw(
  file: File,
  width: number,
  height: number,
): Promise<Uint8Array> {
  const image = await loadImageElement(file);
  const canvas = document.createElement('canvas');
  canvas.width = width;
  canvas.height = height;
  const ctx = canvas.getContext('2d', { willReadFrequently: true });
  if (!ctx) throw new Error('当前浏览器不支持 Canvas，无法转换背景图片');
  const rect = coverFitRect(image.naturalWidth, image.naturalHeight, width, height);
  ctx.clearRect(0, 0, width, height);
  ctx.drawImage(image, rect.offsetX, rect.offsetY, rect.drawWidth, rect.drawHeight);
  const pixels = ctx.getImageData(0, 0, width, height);
  return wrapBackgroundRaw(rgbaToRgb565(pixels.data), width, height);
}

/** 处理后的表情资源 */
export interface EmojiAssetResult {
  data: Uint8Array;
  width: number;
  height: number;
  /** 实际落盘扩展名（浏览器转码后可能变化，如 webp → png） */
  ext: string;
  isGif: boolean;
}

/**
 * 处理单个表情文件：
 * - GIF 原样上传（需要 PSRAM，页面另行提示）；
 * - 静态图超过 64×64 时用 Canvas 等比缩小到 64 以内（PNG 保留透明）；
 * - 已小于 64 的静态图保留原始字节，避免无谓转码。
 */
export async function emojiFileToAssets(file: File): Promise<EmojiAssetResult> {
  if (!isSupportedEmojiFile(file)) {
    throw new Error('仅支持 PNG/GIF/JPG/WEBP 表情图片');
  }
  const ext = fileExtension(file.name);
  const gif = isGifFile(file);
  const image = await loadImageElement(file);
  const width = image.naturalWidth || image.width;
  const height = image.naturalHeight || image.height;

  if (gif) {
    return { data: await readFileBytes(file), width, height, ext: 'gif', isGif: true };
  }
  if (width <= MAX_EMOJI_SIZE && height <= MAX_EMOJI_SIZE) {
    return { data: await readFileBytes(file), width, height, ext, isGif: false };
  }

  const scale = Math.min(MAX_EMOJI_SIZE / width, MAX_EMOJI_SIZE / height);
  const targetWidth = Math.max(1, Math.round(width * scale));
  const targetHeight = Math.max(1, Math.round(height * scale));
  const canvas = document.createElement('canvas');
  canvas.width = targetWidth;
  canvas.height = targetHeight;
  const ctx = canvas.getContext('2d');
  if (!ctx) throw new Error('当前浏览器不支持 Canvas，无法缩放表情图片');
  if (ext !== 'png') {
    ctx.fillStyle = '#ffffff';
    ctx.fillRect(0, 0, targetWidth, targetHeight);
  }
  ctx.drawImage(image, 0, 0, targetWidth, targetHeight);

  const mime = ext === 'webp' ? 'image/webp' : ext === 'png' ? 'image/png' : 'image/jpeg';
  const blob = await canvasToBlob(canvas, mime);
  const actualExt = mimeToExtension(blob.type) || ext;
  return {
    data: await readFileBytes(blob),
    width: targetWidth,
    height: targetHeight,
    ext: actualExt,
    isGif: false,
  };
}

/** Blob MIME → 扩展名；未知时返回空串 */
function mimeToExtension(mime: string): string {
  switch (mime) {
    case 'image/png':
      return 'png';
    case 'image/jpeg':
      return 'jpg';
    case 'image/webp':
      return 'webp';
    case 'image/gif':
      return 'gif';
    default:
      return '';
  }
}

/**
 * 把 assets 接口错误映射为可操作的中文提示。
 * 后端错误码见 `src/web/api/xiaozhi_assets`：invalid_request / invalid_assets /
 * assets_too_large / device_unknown / device_offline / mcp_unavailable / apply_failed。
 */
export function describeAssetsError(error: unknown): string {
  if (!(error instanceof XiaozhiAssetsError)) {
    return '操作失败：请检查网络连接或 haimen 后端日志。';
  }
  const detail = error.detail ? `（${error.detail}）` : '';
  switch (error.code) {
    case 'invalid_request':
      return `请求参数不合法：请重新选择设备后重试${detail}。`;
    case 'invalid_assets':
      return `assets.bin 校验失败：文件头/文件表/checksum 不正确，请重新生成主题包${detail}。`;
    case 'assets_too_large':
      return `assets.bin 超过设备 assets 分区大小：请减少表情或降低背景分辨率${detail}。`;
    case 'device_unknown':
      return `未找到该设备：请先在「小智设备」中登记设备并确认 MAC${detail}。`;
    case 'device_offline':
      return `设备离线：请确认设备已开机并连上 haimen 的 MQTT/UDP 后重试${detail}。`;
    case 'mcp_unavailable':
      return `设备 MCP 控制面不可用：请确认固件支持 assets 下发（MCP 未连接或工具缺失）${detail}。`;
    case 'assets_unavailable':
      return `assets 存储不可用：请检查 haimen 后端 assets 目录是否可写${detail}。`;
    case 'assets_not_found':
      return `assets 文件不存在：可能已被清理，请重新上传${detail}。`;
    case 'upload_failed':
      return `上传失败：请检查网络后重试${detail}。`;
    case 'apply_failed':
      return `下发主题失败：设备可能未响应，请稍后重试${detail}。`;
    default:
      return `操作失败（${error.code}）：${error.message}${detail}`;
  }
}
