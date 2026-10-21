import { describe, expect, it } from 'vitest';
import { XiaozhiAssetsError } from '@/api/xiaozhiAssets';
import {
  ASSETS_HEADER_SIZE,
  ASSETS_NAME_LENGTH,
  ASSETS_TABLE_ENTRY_SIZE,
  buildAssetsBin,
  buildIndexJson,
  buildLvglImageHeader,
  coverFitRect,
  describeAssetsError,
  EMOJI_NAMES,
  EMOJI_PRESETS,
  fileExtension,
  formatBytes,
  isGifFile,
  isSupportedEmojiFile,
  LVGL_IMAGE_HEADER_SIZE,
  presetEmojiUrl,
  rgbaToRgb565,
  wrapBackgroundRaw,
} from './xiaozhiAssets';

function viewOf(bytes: Uint8Array): DataView {
  return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}

function readU32(bytes: Uint8Array, offset: number): number {
  return viewOf(bytes).getUint32(offset, true);
}

function readU16(bytes: Uint8Array, offset: number): number {
  return viewOf(bytes).getUint16(offset, true);
}

/** 读取文件表第 index 项的名字（去掉 0 填充） */
function entryName(bin: Uint8Array, index: number): string {
  const start = ASSETS_HEADER_SIZE + index * ASSETS_TABLE_ENTRY_SIZE;
  const raw = bin.subarray(start, start + ASSETS_NAME_LENGTH);
  const end = raw.indexOf(0);
  return new TextDecoder().decode(end === -1 ? raw : raw.subarray(0, end));
}

/** 文件表第 index 项固定偏移 */
function entryOffset(index: number): number {
  return ASSETS_HEADER_SIZE + index * ASSETS_TABLE_ENTRY_SIZE;
}

const INDEX = { version: 1, hide_subtitle: true };
const INDEX_BYTES = new TextEncoder().encode(JSON.stringify(INDEX, null, 2));

/** 校验文件头并返回 combined 数据区 */
function expectHeader(bin: Uint8Array, totalFiles: number): Uint8Array {
  expect(readU32(bin, 0)).toBe(totalFiles);
  const combinedLen = readU32(bin, 8);
  expect(bin.byteLength).toBe(ASSETS_HEADER_SIZE + combinedLen);
  expect(combinedLen).toBe(bin.byteLength - ASSETS_HEADER_SIZE);
  const combined = bin.subarray(ASSETS_HEADER_SIZE);
  const checksum = combined.reduce((total, byte) => total + byte, 0) & 0xffff;
  expect(readU32(bin, 4)).toBe(checksum);
  return combined;
}

describe('官方表情预设', () => {
  it('提供 Twemoji 64 PNG 与 Noto Emoji 64 GIF 两套', () => {
    expect(EMOJI_PRESETS.map((preset) => [preset.id, preset.format, preset.size])).toEqual([
      ['twemoji64', 'png', 64],
      ['noto-emoji_64', 'gif', 64],
    ]);
  });

  it('预设 URL 指向内置 public/emojis 目录', () => {
    expect(presetEmojiUrl(EMOJI_PRESETS[0], 'happy')).toBe('/emojis/twemoji64/happy.png');
    expect(presetEmojiUrl(EMOJI_PRESETS[1], 'neutral')).toBe('/emojis/noto-emoji_64/neutral.gif');
  });
});

describe('buildAssetsBin', () => {
  it('只包含 index.json 时生成 12 字节头 + 44 字节表 + data', () => {
    const bin = buildAssetsBin([], INDEX);
    const combined = expectHeader(bin, 1);
    // combined = 表(44) + 0x5A5A + index.json
    expect(combined.byteLength).toBe(ASSETS_TABLE_ENTRY_SIZE + 2 + INDEX_BYTES.byteLength);
    expect(entryName(bin, 0)).toBe('index.json');
    expect(readU32(bin, entryOffset(0) + ASSETS_NAME_LENGTH)).toBe(INDEX_BYTES.byteLength);
    expect(readU32(bin, entryOffset(0) + ASSETS_NAME_LENGTH + 4)).toBe(0);
    expect(readU16(bin, entryOffset(0) + ASSETS_NAME_LENGTH + 8)).toBe(0);
    expect(readU16(bin, entryOffset(0) + ASSETS_NAME_LENGTH + 10)).toBe(0);
    // index.json 数据前的 0x5A5A
    expect(bin[ASSETS_HEADER_SIZE + ASSETS_TABLE_ENTRY_SIZE]).toBe(0x5a);
    expect(bin[ASSETS_HEADER_SIZE + ASSETS_TABLE_ENTRY_SIZE + 1]).toBe(0x5a);
  });

  it('文件表记录 size / offset / width / height，offset 指向 0x5A5A 前缀', () => {
    const emoji = {
      name: 'emoji_happy.png',
      data: new Uint8Array([1, 2, 3, 4]),
      width: 64,
      height: 48,
    };
    const font = { name: 'font_custom.bin', data: new Uint8Array([9, 8]) };
    const bin = buildAssetsBin([emoji, font], INDEX);
    expectHeader(bin, 3);
    expect(entryName(bin, 0)).toBe('index.json');
    expect(entryName(bin, 1)).toBe('emoji_happy.png');
    expect(entryName(bin, 2)).toBe('font_custom.bin');

    // 表项：name[32] + size u32 + offset u32 + width u16 + height u16
    expect(readU32(bin, entryOffset(1) + ASSETS_NAME_LENGTH)).toBe(4);
    expect(readU32(bin, entryOffset(1) + ASSETS_NAME_LENGTH + 4)).toBe(2 + INDEX_BYTES.byteLength);
    expect(readU16(bin, entryOffset(1) + ASSETS_NAME_LENGTH + 8)).toBe(64);
    expect(readU16(bin, entryOffset(1) + ASSETS_NAME_LENGTH + 10)).toBe(48);

    expect(readU32(bin, entryOffset(2) + ASSETS_NAME_LENGTH)).toBe(2);
    expect(readU32(bin, entryOffset(2) + ASSETS_NAME_LENGTH + 4)).toBe(
      2 + INDEX_BYTES.byteLength + 2 + 4,
    );
    expect(readU16(bin, entryOffset(2) + ASSETS_NAME_LENGTH + 8)).toBe(0);
    expect(readU16(bin, entryOffset(2) + ASSETS_NAME_LENGTH + 10)).toBe(0);

    // 数据区：index → emoji（前缀 + 4 字节）→ font（前缀 + 2 字节）
    const dataStart = ASSETS_HEADER_SIZE + 3 * ASSETS_TABLE_ENTRY_SIZE;
    expect(bin[dataStart]).toBe(0x5a);
    expect(bin[dataStart + 1]).toBe(0x5a);
    const emojiOffset = dataStart + 2 + INDEX_BYTES.byteLength;
    expect(Array.from(bin.subarray(emojiOffset, emojiOffset + 6))).toEqual([
      0x5a, 0x5a, 1, 2, 3, 4,
    ]);
    const fontOffset = emojiOffset + 6;
    expect(Array.from(bin.subarray(fontOffset, fontOffset + 4))).toEqual([0x5a, 0x5a, 9, 8]);
  });

  it('checksum 覆盖文件表与数据区（不包含 12 字节头）', () => {
    const bin = buildAssetsBin([{ name: 'a.raw', data: new Uint8Array([0xff, 0x01]) }], INDEX);
    const combined = bin.subarray(ASSETS_HEADER_SIZE);
    let sum = 0;
    for (const byte of combined) sum += byte;
    expect(readU32(bin, 4)).toBe(sum & 0xffff);
    expect(readU32(bin, 8)).toBe(combined.byteLength);
  });

  it('index.json 始终排第一，调用方重复传入也不会重复打包', () => {
    const bin = buildAssetsBin([{ name: 'index.json', data: new Uint8Array([7, 7, 7]) }], {
      version: 2,
    });
    expectHeader(bin, 1);
    expect(entryName(bin, 0)).toBe('index.json');
    const dataStart = ASSETS_HEADER_SIZE + ASSETS_TABLE_ENTRY_SIZE;
    expect(bin[dataStart]).toBe(0x5a);
    // 写入的是重新序列化的 index，而不是调用方那份 [7,7,7]
    const serialized = new TextEncoder().encode(JSON.stringify({ version: 2 }, null, 2));
    expect(Array.from(bin.subarray(dataStart + 2, dataStart + 2 + serialized.length))).toEqual(
      Array.from(serialized),
    );
  });

  it('文件名超过 32 字节时截断并补 0', () => {
    const longName = 'emoji_this_name_is_far_too_long_for_the_table.png';
    const bin = buildAssetsBin([{ name: longName, data: new Uint8Array([1]) }], INDEX);
    const start = entryOffset(1);
    const nameBytes = bin.subarray(start, start + ASSETS_NAME_LENGTH);
    expect(Array.from(nameBytes)).toEqual(
      Array.from(new TextEncoder().encode(longName).subarray(0, ASSETS_NAME_LENGTH)),
    );
    // 表格中仍然只占用 44 字节
    expect(readU32(bin, start + ASSETS_NAME_LENGTH)).toBe(1);
  });

  it('空文件也写入 size=0 与数据前缀', () => {
    const bin = buildAssetsBin([{ name: 'empty.bin', data: new Uint8Array(0) }], INDEX);
    expect(readU32(bin, entryOffset(1) + ASSETS_NAME_LENGTH)).toBe(0);
    const dataStart = ASSETS_HEADER_SIZE + 2 * ASSETS_TABLE_ENTRY_SIZE;
    const emptyOffset = dataStart + 2 + INDEX_BYTES.byteLength;
    expect(bin[emptyOffset]).toBe(0x5a);
    expect(bin[emptyOffset + 1]).toBe(0x5a);
    expect(bin.byteLength).toBe(emptyOffset + 2);
  });

  it('text_font 与 emoji_collection 会进入 index.json', () => {
    const bin = buildAssetsBin([], {
      version: 1,
      text_font: 'font_custom.bin',
      emoji_collection: [{ name: 'neutral', file: 'emoji_neutral.gif' }],
    });
    const dataStart = ASSETS_HEADER_SIZE + ASSETS_TABLE_ENTRY_SIZE;
    const indexText = new TextDecoder().decode(bin.subarray(dataStart + 2, bin.byteLength));
    expect(indexText).toContain('font_custom.bin');
    expect(indexText).toContain('emoji_neutral.gif');
  });
});

describe('buildLvglImageHeader', () => {
  it('写入 magic / cf / flags / 宽高 / stride / data_size / 指针占位', () => {
    const header = buildLvglImageHeader(320, 240, 320 * 240 * 2);
    expect(header).toHaveLength(LVGL_IMAGE_HEADER_SIZE);
    expect(header[0]).toBe(0x19);
    expect(header[1]).toBe(0x12);
    expect(readU16(header, 2)).toBe(0);
    expect(readU16(header, 4)).toBe(320);
    expect(readU16(header, 6)).toBe(240);
    expect(readU16(header, 8)).toBe(640);
    expect(readU16(header, 10)).toBe(0);
    expect(readU32(header, 12)).toBe(320 * 240 * 2);
    // data 指针占位固定为 28（相对偏移）
    expect(readU32(header, 16)).toBe(28);
    expect(readU32(header, 20)).toBe(0);
    expect(readU32(header, 24)).toBe(0);
  });

  it('结构固定为 28 字节', () => {
    expect(buildLvglImageHeader(1, 1, 2)).toHaveLength(28);
  });
});

describe('wrapBackgroundRaw', () => {
  it('28 字节头后面跟 RGB565 像素，总长度 = 头 + 宽*高*2', () => {
    const width = 4;
    const height = 3;
    const pixels = new Uint8Array(width * height * 2).fill(0xab);
    const raw = wrapBackgroundRaw(pixels, width, height);
    expect(raw).toHaveLength(LVGL_IMAGE_HEADER_SIZE + width * height * 2);
    expect(raw.subarray(0, 4)).toEqual(new Uint8Array([0x19, 0x12, 0x00, 0x00]));
    expect(readU16(raw, 4)).toBe(width);
    expect(readU16(raw, 6)).toBe(height);
    expect(readU16(raw, 8)).toBe(width * 2);
    expect(readU32(raw, 12)).toBe(width * height * 2);
    expect(raw.subarray(LVGL_IMAGE_HEADER_SIZE)).toEqual(pixels);
  });
});

describe('rgbaToRgb565', () => {
  it('按 5-6-5 位宽转换并输出小端字节', () => {
    const rgba = new Uint8ClampedArray([
      255,
      0,
      0,
      255, // 红 F800
      0,
      255,
      0,
      255, // 绿 07E0
      0,
      0,
      255,
      255, // 蓝 001F
      255,
      255,
      255,
      255, // 白 FFFF
      0,
      0,
      0,
      255, // 黑 0000
    ]);
    expect(Array.from(rgbaToRgb565(rgba))).toEqual([
      0x00, 0xf8, 0xe0, 0x07, 0x1f, 0x00, 0xff, 0xff, 0x00, 0x00,
    ]);
  });
});

describe('coverFitRect', () => {
  it('同比例图片铺满，无偏移', () => {
    expect(coverFitRect(640, 480, 320, 240)).toEqual({
      offsetX: 0,
      offsetY: 0,
      drawWidth: 320,
      drawHeight: 240,
    });
  });

  it('竖图按宽度铺满并上下裁切，offsetY 为负', () => {
    const rect = coverFitRect(100, 200, 320, 240);
    expect(rect.drawWidth).toBe(320);
    expect(rect.drawHeight).toBe(640);
    expect(rect.offsetX).toBe(0);
    expect(rect.offsetY).toBe(-200);
  });

  it('横图按高度铺满并左右裁切，offsetX 为负', () => {
    const rect = coverFitRect(400, 100, 320, 240);
    expect(rect.drawWidth).toBe(960);
    expect(rect.drawHeight).toBe(240);
    expect(rect.offsetX).toBe(-320);
    expect(rect.offsetY).toBe(0);
  });

  it('非法尺寸回退到直接铺满目标尺寸', () => {
    expect(coverFitRect(0, 0, 320, 240)).toEqual({
      offsetX: 0,
      offsetY: 0,
      drawWidth: 320,
      drawHeight: 240,
    });
  });
});

describe('buildIndexJson', () => {
  const base = {
    screen: { width: 320, height: 240, monochrome: false },
    hideSubtitle: false,
  };

  it('默认不写 text_font / emoji_collection，保留皮肤纯色背景', () => {
    const index = buildIndexJson({
      ...base,
      light: { mode: 'solid', color: '#ffffff', hasImage: false },
      dark: { mode: 'solid', color: '#121212', hasImage: false },
    });
    expect(index.version).toBe(1);
    expect(index.display_config).toEqual({
      width: 320,
      height: 240,
      monochrome: false,
      color: 'RGB565',
    });
    expect(index.skin?.light).toEqual({
      text_color: '#000000',
      background_color: '#ffffff',
    });
    expect(index.skin?.dark).toEqual({
      text_color: '#ffffff',
      background_color: '#121212',
    });
    expect(index).not.toHaveProperty('text_font');
    expect(index).not.toHaveProperty('emoji_collection');
  });

  it('图片模式且已生成 raw 时写入 background_light/dark.raw', () => {
    const index = buildIndexJson({
      ...base,
      hideSubtitle: true,
      light: { mode: 'image', color: '#000000', hasImage: true },
      dark: { mode: 'image', color: '#102030', hasImage: true },
    });
    expect(index.hide_subtitle).toBe(true);
    expect(index.skin?.light?.background_image).toBe('background_light.raw');
    expect(index.skin?.dark?.background_image).toBe('background_dark.raw');
  });

  it('图片模式但 raw 为空（转换失败）时不写 background_image', () => {
    const index = buildIndexJson({
      ...base,
      light: { mode: 'image', color: '#000000', hasImage: false },
      dark: { mode: 'solid', color: '#121212', hasImage: false },
    });
    expect(index.skin?.light).not.toHaveProperty('background_image');
  });

  it('text_font / emoji_collection 可选写入，空 file 被过滤', () => {
    const index = buildIndexJson({
      ...base,
      light: { mode: 'solid', color: '#ffffff', hasImage: false },
      dark: { mode: 'solid', color: '#121212', hasImage: false },
      textFont: 'font_custom.bin',
      emojiCollection: [
        { name: 'neutral', file: 'emoji_neutral.gif' },
        { name: 'happy', file: '' },
      ],
    });
    expect(index.text_font).toBe('font_custom.bin');
    expect(index.emoji_collection).toEqual([{ name: 'neutral', file: 'emoji_neutral.gif' }]);
  });

  it('单色屏透传 monochrome', () => {
    const index = buildIndexJson({
      ...base,
      screen: { width: 128, height: 64, monochrome: true },
      light: { mode: 'solid', color: '#ffffff', hasImage: false },
      dark: { mode: 'solid', color: '#121212', hasImage: false },
    });
    expect(index.display_config.monochrome).toBe(true);
  });
});

describe('表情槽与文件辅助函数', () => {
  it('固定 21 个表情名且包含 neutral', () => {
    expect(EMOJI_NAMES).toHaveLength(21);
    expect(EMOJI_NAMES[0]).toBe('neutral');
    expect(new Set(EMOJI_NAMES).size).toBe(21);
  });

  it('fileExtension 取小写扩展名', () => {
    expect(fileExtension('emoji_happy.PNG')).toBe('png');
    expect(fileExtension('font_custom.bin')).toBe('bin');
    expect(fileExtension('noext')).toBe('');
  });

  it('isSupportedEmojiFile 只接受 png/gif/jpg/jpeg/webp', () => {
    expect(isSupportedEmojiFile(new File(['a'], 'happy.png'))).toBe(true);
    expect(isSupportedEmojiFile(new File(['a'], 'happy.GIF'))).toBe(true);
    expect(isSupportedEmojiFile(new File(['a'], 'happy.jpg'))).toBe(true);
    expect(isSupportedEmojiFile(new File(['a'], 'happy.webp'))).toBe(true);
    expect(isSupportedEmojiFile(new File(['a'], 'happy.svg'))).toBe(false);
    expect(isSupportedEmojiFile(new File(['a'], 'happy.bin'))).toBe(false);
  });

  it('isGifFile 按扩展名或 MIME 识别 GIF', () => {
    expect(isGifFile(new File(['a'], 'happy.gif'))).toBe(true);
    expect(isGifFile(new File(['a'], 'happy.bin', { type: 'image/gif' }))).toBe(true);
    expect(isGifFile(new File(['a'], 'happy.png', { type: 'image/png' }))).toBe(false);
  });
});

describe('formatBytes', () => {
  it('按 B / KB / MB 展示', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(2048)).toBe('2.0 KB');
    expect(formatBytes(2 * 1024 * 1024)).toBe('2.00 MB');
  });

  it('非法值回退未知大小', () => {
    expect(formatBytes(-1)).toBe('未知大小');
    expect(formatBytes(Number.NaN)).toBe('未知大小');
  });
});

describe('describeAssetsError', () => {
  it('按错误码给出中文提示', () => {
    expect(describeAssetsError(new XiaozhiAssetsError('device_offline', 409, 'offline'))).toContain(
      '设备离线',
    );
    expect(describeAssetsError(new XiaozhiAssetsError('invalid_assets', 400, 'bad'))).toContain(
      '校验失败',
    );
    expect(describeAssetsError(new XiaozhiAssetsError('mcp_unavailable', 503, 'no mcp'))).toContain(
      'MCP',
    );
    expect(
      describeAssetsError(new XiaozhiAssetsError('device_offline', 409, 'offline', 'mac 未连接')),
    ).toContain('mac 未连接');
  });

  it('未知错误码保留 code 与 message', () => {
    const message = describeAssetsError(new XiaozhiAssetsError('weird_error', 500, 'boom'));
    expect(message).toContain('weird_error');
    expect(message).toContain('boom');
  });

  it('非 XiaozhiAssetsError 回退通用提示', () => {
    expect(describeAssetsError(new Error('network'))).toContain('检查网络');
  });
});
