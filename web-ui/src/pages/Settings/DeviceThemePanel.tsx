import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { getXiaozhiDevices } from '@/api/xiaozhi';
import {
  applyXiaozhiAssets,
  getXiaozhiAssetsDeviceInfo,
  uploadXiaozhiAssets,
  type XiaozhiAssetsDeviceInfo,
  type XiaozhiAssetsUploadResult,
} from '@/api/xiaozhiAssets';
import Combobox from '@/components/Combobox';
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
import { cn } from '@/lib/utils';
import {
  type AssetsFile,
  type AssetsIndexJson,
  buildAssetsBin,
  buildIndexJson,
  DEFAULT_DARK_BACKGROUND,
  DEFAULT_LIGHT_BACKGROUND,
  describeAssetsError,
  EMOJI_NAMES,
  EMOJI_PRESETS,
  type EmojiName,
  type EmojiPresetId,
  emojiFileToAssets,
  formatBytes,
  imageToBackgroundRaw,
  isGifFile,
  presetEmojiUrl,
  readFileBytes,
} from '@/lib/xiaozhiAssets';

/** 后端单次上传上限（与 `src/web/api/xiaozhi_assets` 保持一致） */
const MAX_UPLOAD_BYTES = 8 * 1024 * 1024;

/** 背景图文件名（固件约定） */
const LIGHT_BACKGROUND_FILE = 'background_light.raw';
const DARK_BACKGROUND_FILE = 'background_dark.raw';

/** 自定义字体文件名（固件约定） */
const CUSTOM_FONT_FILE = 'font_custom.bin';

const HEX_COLOR = /^#[0-9a-fA-F]{6}$/;

/** 设备下拉项 */
interface DeviceOption {
  value: string;
  label: string;
}

/** 浅色 / 深色背景编辑状态 */
interface BackgroundDraft {
  mode: 'solid' | 'image';
  color: string;
  file: File | null;
  /** 已转换好的 28 字节头 + RGB565 数据 */
  raw: Uint8Array | null;
  converting: boolean;
  error: string | null;
}

/** 单个表情槽状态 */
interface EmojiDraft {
  file: File | null;
  data: Uint8Array | null;
  width: number;
  height: number;
  ext: string;
  isGif: boolean;
  converting: boolean;
  error: string | null;
}

type BusyPhase = 'idle' | 'uploading' | 'applying';

function createBackgroundDraft(color: string): BackgroundDraft {
  return { mode: 'solid', color, file: null, raw: null, converting: false, error: null };
}

function createEmojiDraft(): EmojiDraft {
  return {
    file: null,
    data: null,
    width: 0,
    height: 0,
    ext: '',
    isGif: false,
    converting: false,
    error: null,
  };
}

function createEmojiDrafts(): Record<EmojiName, EmojiDraft> {
  return Object.fromEntries(EMOJI_NAMES.map((name) => [name, createEmojiDraft()])) as Record<
    EmojiName,
    EmojiDraft
  >;
}

/** 颜色非法时回退到默认值，避免 `<input type="color">` 收到非法值 */
function safeColor(color: string, fallback: string): string {
  return HEX_COLOR.test(color) ? color : fallback;
}

/** 文件名 → 表情展示名（neutral → Neutral） */
function emojiDisplayName(name: EmojiName): string {
  return name.charAt(0).toUpperCase() + name.slice(1);
}

/** 设备展示名：名称为空或等于 MAC 时显示「未命名设备」 */
function deviceDisplayName(name: string, deviceId: string): string {
  const trimmed = name.trim();
  if (!trimmed || trimmed === deviceId) return '未命名设备';
  return trimmed;
}

/** MCP 状态文案：兼容布尔 / `{connected}` / 其他对象三种返回形状 */
function describeMcp(mcp: unknown): string {
  if (mcp == null) return '未上报';
  if (typeof mcp === 'boolean') return mcp ? '已连接' : '未连接';
  if (typeof mcp === 'object') {
    const connected = (mcp as Record<string, unknown>).connected;
    if (typeof connected === 'boolean') return connected ? '已连接' : '未连接';
    return '已上报';
  }
  return String(mcp);
}

/**
 * 生成 / 复用 File 的预览 URL；file 变化或组件卸载时自动回收。
 * 每个预览组件（背景、表情）各调用一次，避免手动管理 object URL。
 */
function useObjectUrl(file: File | null): string | null {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    if (!file) {
      setUrl(null);
      return;
    }
    const next = URL.createObjectURL(file);
    setUrl(next);
    return () => URL.revokeObjectURL(next);
  }, [file]);
  return url;
}

// ─── 子组件 ─────────────────────────────────────────────────

/** 信息项：左标签右值 */
function InfoItem({
  label,
  value,
  mono = false,
}: {
  label: string;
  value: string;
  mono?: boolean;
}) {
  return (
    <div className="min-w-0 space-y-0.5">
      <p className="text-xs text-muted-foreground">{label}</p>
      <p className={cn('truncate text-sm', mono && 'font-mono text-xs')} title={value}>
        {value}
      </p>
    </div>
  );
}

/** 浅色 / 深色背景编辑器：纯色 / 图片（cover → RGB565） */
function BackgroundEditor({
  title,
  idSuffix,
  fallbackColor,
  draft,
  screen,
  onChange,
  onPickFile,
  disabled,
}: {
  title: string;
  /** 用于生成 htmlFor / id，避免浅色与深色控件 ID 冲突 */
  idSuffix: 'light' | 'dark';
  fallbackColor: string;
  draft: BackgroundDraft;
  screen: { width: number; height: number } | null;
  onChange: (patch: Partial<BackgroundDraft>) => void;
  onPickFile: (file: File) => void;
  disabled: boolean;
}) {
  const inputRef = useRef<HTMLInputElement>(null);
  const previewUrl = useObjectUrl(draft.file);
  const colorValid = HEX_COLOR.test(draft.color);

  return (
    <div className="space-y-3 rounded-lg border p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="text-sm font-medium">{title}</span>
        <fieldset className="flex gap-1" aria-label={`${title}背景模式`}>
          <Button
            type="button"
            size="xs"
            variant={draft.mode === 'solid' ? 'default' : 'outline'}
            onClick={() => onChange({ mode: 'solid' })}
            disabled={disabled}
          >
            纯色
          </Button>
          <Button
            type="button"
            size="xs"
            variant={draft.mode === 'image' ? 'default' : 'outline'}
            onClick={() => onChange({ mode: 'image' })}
            disabled={disabled || !screen}
            title={screen ? undefined : '设备未上报屏幕分辨率，无法转换背景图片'}
          >
            图片
          </Button>
        </fieldset>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <label className="text-xs text-muted-foreground" htmlFor={`theme-bg-${idSuffix}-color`}>
          背景色
        </label>
        <input
          id={`theme-bg-${idSuffix}-color`}
          type="color"
          aria-label={`${title}背景色选择器`}
          value={safeColor(draft.color, fallbackColor)}
          onChange={(event) => onChange({ color: event.target.value })}
          disabled={disabled}
          className="h-9 w-12 cursor-pointer rounded-md border border-input bg-transparent p-1"
        />
        <Input
          aria-label={`${title}背景色十六进制值`}
          value={draft.color}
          onChange={(event) => onChange({ color: event.target.value.trim() })}
          placeholder={fallbackColor}
          className="w-28 font-mono"
          disabled={disabled}
        />
        {!colorValid && <span className="text-xs text-destructive">格式应为 #RRGGBB</span>}
      </div>

      {draft.mode === 'image' && (
        <div className="space-y-2">
          <div className="flex flex-wrap items-center gap-2">
            <input
              ref={inputRef}
              type="file"
              accept="image/png,image/jpeg,image/webp,image/gif"
              aria-label={`${title}图片文件`}
              className="hidden"
              onChange={(event) => {
                const file = event.target.files?.[0];
                event.target.value = '';
                if (file) onPickFile(file);
              }}
            />
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => inputRef.current?.click()}
              disabled={disabled || !screen || draft.converting}
            >
              {draft.file ? '更换图片' : '选择图片'}
            </Button>
            {draft.file && (
              <Button
                type="button"
                variant="ghost"
                size="sm"
                onClick={() => onChange({ file: null, raw: null, error: null })}
                disabled={disabled || draft.converting}
              >
                移除
              </Button>
            )}
            <span className="min-w-0 truncate text-xs text-muted-foreground">
              {draft.file?.name ?? (screen ? '未选择' : '需先获取屏幕分辨率')}
            </span>
          </div>
          {draft.converting && (
            <p className="text-xs text-muted-foreground">正在转换为 RGB565...</p>
          )}
          {draft.error && <p className="text-xs text-destructive">{draft.error}</p>}
          {draft.raw && (
            <div className="flex items-center gap-2">
              {previewUrl && (
                <img
                  src={previewUrl}
                  alt={`${title}预览`}
                  className="h-20 w-auto rounded-md border object-cover"
                />
              )}
              <span className="text-xs text-muted-foreground">
                已转换 {formatBytes(draft.raw.byteLength)}（{screen?.width}×{screen?.height} cover）
              </span>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/** 单个表情槽：预览 + 上传 / 移除 */
function EmojiSlot({
  name,
  draft,
  disabled,
  onPickFile,
  onClear,
}: {
  name: EmojiName;
  draft: EmojiDraft;
  disabled: boolean;
  onPickFile: (file: File) => void;
  onClear: () => void;
}) {
  const inputRef = useRef<HTMLInputElement>(null);
  const previewUrl = useObjectUrl(draft.file);

  return (
    <div className="flex items-center gap-2 rounded-lg border p-2">
      <div className="flex h-12 w-12 shrink-0 items-center justify-center overflow-hidden rounded-md border bg-muted">
        {previewUrl ? (
          <img
            src={previewUrl}
            alt={`${emojiDisplayName(name)} 预览`}
            className="max-h-12 max-w-12 object-contain"
          />
        ) : (
          <span className="text-[10px] text-muted-foreground">未设置</span>
        )}
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-1">
          <span className="truncate text-xs font-medium">{emojiDisplayName(name)}</span>
          <span className="font-mono text-[10px] text-muted-foreground">{name}</span>
          {name === 'neutral' && (
            <Badge variant="outline" className="px-1 py-0 text-[10px]">
              必填
            </Badge>
          )}
          {draft.isGif && (
            <Badge variant="outline" className="px-1 py-0 text-[10px]">
              GIF
            </Badge>
          )}
        </div>
        <p className="truncate text-[11px] text-muted-foreground" title={draft.error ?? undefined}>
          {draft.converting
            ? '处理中...'
            : draft.error
              ? draft.error
              : draft.file
                ? `${draft.file.name} · ${draft.width}×${draft.height}`
                : '未设置'}
        </p>
      </div>
      <input
        ref={inputRef}
        type="file"
        accept="image/png,image/gif,image/jpeg,image/webp"
        aria-label={`上传 ${emojiDisplayName(name)} 表情`}
        className="hidden"
        onChange={(event) => {
          const file = event.target.files?.[0];
          event.target.value = '';
          if (file) onPickFile(file);
        }}
      />
      <div className="flex shrink-0 flex-col gap-1">
        <Button
          type="button"
          variant="outline"
          size="xs"
          onClick={() => inputRef.current?.click()}
          disabled={disabled || draft.converting}
          aria-label={`选择 ${emojiDisplayName(name)} 表情图片`}
        >
          {draft.file ? '更换' : '上传'}
        </Button>
        {draft.file && (
          <Button
            type="button"
            variant="ghost"
            size="xs"
            onClick={onClear}
            disabled={disabled || draft.converting}
            aria-label={`移除 ${emojiDisplayName(name)} 表情`}
          >
            移除
          </Button>
        )}
      </div>
    </div>
  );
}

// ─── 主面板 ─────────────────────────────────────────────────

/**
 * 自定义主题面板。
 *
 * 流程：选择设备 → 读取 device-info（分辨率 / assets 分区）→ 编辑背景、
 * 表情、字体、字幕 → 浏览器内打包 assets.bin（12 字节头 + 44 字节文件表 +
 * 0x5A5A 数据）→ 上传拿文件名 → 通过 MCP 下发 `self.assets.set_download_url`
 * 并重启设备。设备信息失败只影响本卡片，不阻塞上面的设备表。
 */
export default function DeviceThemePanel() {
  const [devices, setDevices] = useState<DeviceOption[]>([]);
  const [devicesLoading, setDevicesLoading] = useState(true);
  const [devicesError, setDevicesError] = useState<string | null>(null);
  const [selectedDevice, setSelectedDevice] = useState<string | null>(null);

  const [info, setInfo] = useState<XiaozhiAssetsDeviceInfo | null>(null);
  const [infoLoading, setInfoLoading] = useState(false);
  const [infoError, setInfoError] = useState<string | null>(null);
  const [infoToken, setInfoToken] = useState(0);

  const [light, setLight] = useState<BackgroundDraft>(() =>
    createBackgroundDraft(DEFAULT_LIGHT_BACKGROUND),
  );
  const [dark, setDark] = useState<BackgroundDraft>(() =>
    createBackgroundDraft(DEFAULT_DARK_BACKGROUND),
  );
  const [emojis, setEmojis] = useState<Record<EmojiName, EmojiDraft>>(createEmojiDrafts);
  const [emojiPreset, setEmojiPreset] = useState<EmojiPresetId | 'none'>('twemoji64');
  const [presetLoading, setPresetLoading] = useState(false);
  const [presetError, setPresetError] = useState<string | null>(null);
  const presetRequest = useRef(0);
  const [fontFile, setFontFile] = useState<File | null>(null);
  const [fontData, setFontData] = useState<Uint8Array | null>(null);
  const [fontConverting, setFontConverting] = useState(false);
  const [fontError, setFontError] = useState<string | null>(null);
  const [hideSubtitle, setHideSubtitle] = useState(false);

  const [phase, setPhase] = useState<BusyPhase>('idle');
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [uploaded, setUploaded] = useState<XiaozhiAssetsUploadResult | null>(null);

  const backgroundRequest = useRef<{ light: number; dark: number }>({ light: 0, dark: 0 });
  const emojiRequest = useRef<Record<string, number>>({});
  const fontInputRef = useRef<HTMLInputElement>(null);

  const loadDevices = useCallback(async () => {
    setDevicesLoading(true);
    setDevicesError(null);
    try {
      const data = await getXiaozhiDevices();
      const options: DeviceOption[] = [];
      const seen = new Set<string>();
      for (const device of data.devices) {
        seen.add(device.device_id);
        options.push({
          value: device.device_id,
          label: `${deviceDisplayName(device.name, device.device_id)} · ${device.device_id}`,
        });
      }
      // 在线但未登记的设备也可以下发主题，一并放进下拉框
      for (const device of data.online) {
        if (seen.has(device.device_id)) continue;
        seen.add(device.device_id);
        options.push({
          value: device.device_id,
          label: `${deviceDisplayName(device.name, device.device_id)} · ${device.device_id}（在线未登记）`,
        });
      }
      setDevices(options);
      setSelectedDevice((prev) =>
        prev && options.some((option) => option.value === prev)
          ? prev
          : (options[0]?.value ?? null),
      );
    } catch {
      setDevicesError('加载设备列表失败：请检查网络连接或 haimen 后端日志。');
    } finally {
      setDevicesLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadDevices();
  }, [loadDevices]);

  // 选择设备后读取 device-info；切换设备时中止上一次请求
  useEffect(() => {
    if (!selectedDevice) {
      setInfo(null);
      setInfoError(null);
      setInfoLoading(false);
      return;
    }
    const controller = new AbortController();
    let cancelled = false;
    // infoToken 是「刷新设备信息」按钮的触发器：变化时重新请求一次
    const requestToken = infoToken;
    setInfoLoading(true);
    setInfoError(null);
    getXiaozhiAssetsDeviceInfo(selectedDevice, controller.signal)
      .then((data) => {
        if (!cancelled && requestToken === infoToken) setInfo(data);
      })
      .catch((error: unknown) => {
        if (cancelled || (error instanceof DOMException && error.name === 'AbortError')) return;
        setInfo(null);
        setInfoError(describeAssetsError(error));
      })
      .finally(() => {
        if (!cancelled) setInfoLoading(false);
      });
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [selectedDevice, infoToken]);

  /** 切换设备：分辨率可能不同，清空已转换的背景图（背景色保留） */
  const handleSelectDevice = (value: string) => {
    if (value === selectedDevice) return;
    setSelectedDevice(value);
    setInfo(null);
    setNotice(null);
    setActionError(null);
    setUploaded(null);
    setLight((prev) => ({ ...prev, file: null, raw: null, converting: false, error: null }));
    setDark((prev) => ({ ...prev, file: null, raw: null, converting: false, error: null }));
    setEmojis((prev) => {
      // 表情与分辨率无关，但重新选设备时清掉上一次的转换错误，便于重试
      const next = { ...prev };
      for (const name of EMOJI_NAMES) next[name] = { ...next[name], error: null };
      return next;
    });
  };

  const refresh = () => {
    void loadDevices();
    setInfoToken((token) => token + 1);
  };

  const updateBackground = (mode: 'light' | 'dark', patch: Partial<BackgroundDraft>) => {
    if (mode === 'light') setLight((prev) => ({ ...prev, ...patch }));
    else setDark((prev) => ({ ...prev, ...patch }));
  };

  const handleBackgroundFile = async (mode: 'light' | 'dark', file: File) => {
    const seq = backgroundRequest.current[mode] + 1;
    backgroundRequest.current[mode] = seq;
    updateBackground(mode, { mode: 'image', file, raw: null, converting: true, error: null });
    setActionError(null);
    setNotice(null);
    const screen = info?.screen ?? null;
    if (!screen) {
      updateBackground(mode, {
        converting: false,
        error: '设备未上报屏幕分辨率，无法转换背景图片',
      });
      return;
    }
    try {
      const raw = await imageToBackgroundRaw(file, screen.width, screen.height);
      if (backgroundRequest.current[mode] !== seq) return;
      updateBackground(mode, { raw, converting: false });
    } catch (error) {
      if (backgroundRequest.current[mode] !== seq) return;
      updateBackground(mode, {
        converting: false,
        error: error instanceof Error ? error.message : '背景图片转换失败',
      });
    }
  };

  const updateEmoji = (name: EmojiName, patch: Partial<EmojiDraft>) => {
    setEmojis((prev) => ({ ...prev, [name]: { ...prev[name], ...patch } }));
  };

  const handleEmojiFile = async (name: EmojiName, file: File) => {
    const seq = (emojiRequest.current[name] ?? 0) + 1;
    emojiRequest.current[name] = seq;
    updateEmoji(name, {
      file,
      data: null,
      width: 0,
      height: 0,
      ext: '',
      isGif: isGifFile(file),
      converting: true,
      error: null,
    });
    setActionError(null);
    setNotice(null);
    try {
      const result = await emojiFileToAssets(file);
      if (emojiRequest.current[name] !== seq) return;
      updateEmoji(name, {
        data: result.data,
        width: result.width,
        height: result.height,
        ext: result.ext,
        isGif: result.isGif,
        converting: false,
      });
    } catch (error) {
      if (emojiRequest.current[name] !== seq) return;
      updateEmoji(name, {
        file: null,
        data: null,
        converting: false,
        error: error instanceof Error ? error.message : '处理表情图片失败',
      });
    }
  };

  const clearEmoji = (name: EmojiName) => {
    emojiRequest.current[name] = (emojiRequest.current[name] ?? 0) + 1;
    updateEmoji(name, createEmojiDraft());
  };

  /** 清空全部表情槽：保留设备当前表情，不写 emoji_collection。 */
  const clearEmojiPreset = () => {
    presetRequest.current += 1;
    setEmojiPreset('none');
    setPresetLoading(false);
    setPresetError(null);
    setEmojis(createEmojiDrafts());
  };

  /**
   * 加载内置官方表情包：
   * - Twemoji 64 PNG：静态、体积小，默认；
   * - Noto Emoji 64 GIF：动态，设备需要 PSRAM。
   */
  const loadEmojiPreset = useCallback(async (id: EmojiPresetId) => {
    const preset = EMOJI_PRESETS.find((item) => item.id === id);
    if (!preset) return;
    const seq = presetRequest.current + 1;
    presetRequest.current = seq;
    setEmojiPreset(id);
    setPresetLoading(true);
    setPresetError(null);
    setEmojis((prev) => {
      const next = { ...prev };
      for (const name of EMOJI_NAMES) {
        next[name] = { ...next[name], converting: true, error: null };
      }
      return next;
    });
    try {
      const results = await Promise.all(
        EMOJI_NAMES.map(async (name) => {
          const response = await fetch(presetEmojiUrl(preset, name), { cache: 'force-cache' });
          if (!response.ok) {
            throw new Error(`${name}: HTTP ${response.status}`);
          }
          const blob = await response.blob();
          const file = new File([blob], `${name}.${preset.format}`, {
            type: preset.format === 'gif' ? 'image/gif' : 'image/png',
          });
          const processed = await emojiFileToAssets(file);
          return { name, file, processed };
        }),
      );
      if (presetRequest.current !== seq) return;
      setEmojis((prev) => {
        const next = { ...prev };
        for (const item of results) {
          next[item.name] = {
            file: item.file,
            data: item.processed.data,
            width: item.processed.width,
            height: item.processed.height,
            ext: item.processed.ext,
            isGif: item.processed.isGif,
            converting: false,
            error: null,
          };
        }
        return next;
      });
    } catch (error) {
      if (presetRequest.current !== seq) return;
      setPresetError(
        error instanceof Error
          ? `加载 ${preset.name} 失败：${error.message}`
          : `加载 ${preset.name} 失败`,
      );
      setEmojis((prev) => {
        const next = { ...prev };
        for (const name of EMOJI_NAMES) {
          next[name] = { ...next[name], converting: false };
        }
        return next;
      });
    } finally {
      if (presetRequest.current === seq) setPresetLoading(false);
    }
  }, []);

  // 默认使用官方 Twemoji 64 静态表情；用户可切到 Noto GIF 或“保持当前”。
  useEffect(() => {
    void loadEmojiPreset('twemoji64');
  }, [loadEmojiPreset]);

  const handleFontFile = async (file: File) => {
    setFontError(null);
    setNotice(null);
    setActionError(null);
    if (!file.name.toLowerCase().endsWith('.bin')) {
      setFontFile(null);
      setFontData(null);
      setFontError('请上传预编译的 CBin 字体文件（.bin），不支持 TTF/OTF 转换。');
      return;
    }
    setFontFile(file);
    setFontData(null);
    setFontConverting(true);
    try {
      setFontData(await readFileBytes(file));
    } catch {
      setFontFile(null);
      setFontError('读取字体文件失败，请重试。');
    } finally {
      setFontConverting(false);
    }
  };

  const clearFont = () => {
    setFontFile(null);
    setFontData(null);
    setFontError(null);
    if (fontInputRef.current) fontInputRef.current.value = '';
  };

  const screen = info?.screen ?? null;

  const emojiEntries = useMemo(() => {
    const entries: { name: EmojiName; file: string }[] = [];
    const files: AssetsFile[] = [];
    for (const name of EMOJI_NAMES) {
      const slot = emojis[name];
      if (!slot.data) continue;
      const fileName = `emoji_${name}.${slot.ext}`;
      entries.push({ name, file: fileName });
      files.push({
        name: fileName,
        data: slot.data,
        width: slot.width,
        height: slot.height,
      });
    }
    return { entries, files };
  }, [emojis]);

  const assetFiles = useMemo<AssetsFile[]>(() => {
    const files: AssetsFile[] = [];
    if (light.mode === 'image' && light.raw) {
      files.push({ name: LIGHT_BACKGROUND_FILE, data: light.raw });
    }
    if (dark.mode === 'image' && dark.raw) {
      files.push({ name: DARK_BACKGROUND_FILE, data: dark.raw });
    }
    files.push(...emojiEntries.files);
    if (fontData) files.push({ name: CUSTOM_FONT_FILE, data: fontData });
    return files;
  }, [light.mode, light.raw, dark.mode, dark.raw, emojiEntries.files, fontData]);

  const indexObject = useMemo<AssetsIndexJson | null>(() => {
    if (!screen) return null;
    return buildIndexJson({
      screen,
      light: {
        mode: light.mode,
        color: safeColor(light.color, DEFAULT_LIGHT_BACKGROUND),
        hasImage: light.raw !== null,
      },
      dark: {
        mode: dark.mode,
        color: safeColor(dark.color, DEFAULT_DARK_BACKGROUND),
        hasImage: dark.raw !== null,
      },
      hideSubtitle,
      emojiCollection: emojiEntries.entries,
      textFont: fontData ? CUSTOM_FONT_FILE : undefined,
    });
  }, [
    screen,
    light.mode,
    light.color,
    light.raw,
    dark.mode,
    dark.color,
    dark.raw,
    hideSubtitle,
    emojiEntries.entries,
    fontData,
  ]);

  const assetsBin = useMemo(() => {
    if (!indexObject) return null;
    return buildAssetsBin(assetFiles, indexObject);
  }, [assetFiles, indexObject]);

  const packageEntries = useMemo(() => {
    if (!indexObject) return [];
    const indexSize = new TextEncoder().encode(JSON.stringify(indexObject, null, 2)).byteLength;
    return [
      { name: 'index.json', size: indexSize, width: 0, height: 0 },
      ...assetFiles.map((file) => ({
        name: file.name,
        size: file.data.byteLength,
        width: file.width ?? 0,
        height: file.height ?? 0,
      })),
    ];
  }, [indexObject, assetFiles]);

  const partitionSize = info?.assets_partition_size ?? null;
  const binSize = assetsBin?.byteLength ?? 0;
  const overPartition = partitionSize !== null && binSize > partitionSize;
  const overUploadLimit = binSize > MAX_UPLOAD_BYTES;
  const nearPartitionLimit =
    partitionSize !== null && !overPartition && binSize > partitionSize * 0.9;
  const neutralMissing = emojiEntries.entries.length > 0 && !emojis.neutral.data;
  const hasGif = EMOJI_NAMES.some((name) => emojis[name].isGif);
  const processing =
    light.converting ||
    dark.converting ||
    fontConverting ||
    presetLoading ||
    EMOJI_NAMES.some((name) => emojis[name].converting);

  const blockReason: string | null = !selectedDevice
    ? '请先选择目标设备'
    : infoLoading
      ? '正在读取设备信息...'
      : !info
        ? '请先成功读取设备信息'
        : !screen
          ? '设备未上报屏幕分辨率，无法生成 display_config'
          : neutralMissing
            ? '已上传自定义表情，但缺少必填的 neutral 表情'
            : processing
              ? '正在处理图片或字体，请稍候'
              : overPartition
                ? '主题包超过设备 assets 分区大小'
                : overUploadLimit
                  ? `主题包超过 ${formatBytes(MAX_UPLOAD_BYTES)} 上传上限`
                  : null;

  const applyDisabled = blockReason !== null || phase !== 'idle' || !assetsBin;

  const handleApply = async () => {
    if (!selectedDevice || !assetsBin || applyDisabled) return;
    setActionError(null);
    setNotice(null);
    setUploaded(null);
    setPhase('uploading');
    try {
      const result = await uploadXiaozhiAssets(assetsBin, 'assets.bin');
      setUploaded(result);
      setPhase('applying');
      const applied = await applyXiaozhiAssets(selectedDevice, result.file);
      setNotice(
        [
          `主题已上传并下发：${result.file}（${formatBytes(result.size)}`,
          result.sha256 ? `，sha256 ${result.sha256.slice(0, 12)}…` : '',
          '）。',
          applied.reboot
            ? '设备将重启并拉取新的 assets.bin；重启完成后自动应用，无需手动操作。'
            : '下发成功，但设备未确认重启；如主题未生效请重启设备。',
        ].join(''),
      );
    } catch (error) {
      setActionError(describeAssetsError(error));
    } finally {
      setPhase('idle');
    }
  };

  if (devicesLoading && devices.length === 0 && !devicesError) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">自定义主题</CardTitle>
        </CardHeader>
        <CardContent className="space-y-2">
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-24 w-full" />
          <Skeleton className="h-8 w-full" />
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="flex flex-wrap items-center gap-2 text-base">
          自定义主题
          <span className="text-xs font-normal text-muted-foreground">
            背景 / 表情 / 字体 / 字幕
          </span>
        </CardTitle>
        <CardDescription>
          在浏览器内生成 assets.bin（12 字节头 + 44 字节文件表），通过设备 MCP 下发
          `self.assets.set_download_url` 并重启生效。仅修改 assets 分区，不改固件。
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-5">
        {/* 设备选择与 device-info */}
        <div className="space-y-3">
          <div className="flex flex-wrap items-end gap-2">
            <div className="min-w-64 flex-1 space-y-1.5">
              <span className="text-sm font-medium">目标设备</span>
              <Combobox
                options={devices}
                value={selectedDevice}
                onChange={handleSelectDevice}
                placeholder={devicesLoading ? '加载中...' : '选择要应用主题的设备'}
                searchPlaceholder="搜索设备名或 MAC..."
                emptyText="没有可用的设备"
              />
            </div>
            <Button
              type="button"
              variant="outline"
              onClick={refresh}
              disabled={devicesLoading || infoLoading}
            >
              {devicesLoading || infoLoading ? '刷新中...' : '刷新设备信息'}
            </Button>
          </div>

          {devicesError && (
            <Alert variant="destructive">
              <AlertTitle>加载设备列表失败</AlertTitle>
              <AlertDescription>{devicesError}</AlertDescription>
            </Alert>
          )}

          {!devicesLoading && devices.length === 0 && !devicesError && (
            <Alert>
              <AlertTitle>还没有可用的设备</AlertTitle>
              <AlertDescription>
                请先在「小智设备」中登记设备，并让设备开机联网后重试。
              </AlertDescription>
            </Alert>
          )}

          {selectedDevice && infoLoading && (
            <div className="grid gap-2 rounded-lg border p-3 sm:grid-cols-2 lg:grid-cols-4">
              <Skeleton className="h-8 w-full" />
              <Skeleton className="h-8 w-full" />
              <Skeleton className="h-8 w-full" />
              <Skeleton className="h-8 w-full" />
            </div>
          )}

          {selectedDevice && !infoLoading && infoError && (
            <Alert variant="destructive">
              <AlertTitle>读取设备信息失败</AlertTitle>
              <AlertDescription>{infoError}</AlertDescription>
            </Alert>
          )}

          {selectedDevice && !infoLoading && info && (
            <div className="space-y-2 rounded-lg border p-3">
              <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
                <InfoItem label="设备" value={`${info.device}`} mono />
                <div className="space-y-1">
                  <p className="text-xs text-muted-foreground">在线状态</p>
                  <Badge
                    variant="outline"
                    className={
                      info.online
                        ? 'border-transparent bg-green-500/15 text-green-600'
                        : 'text-muted-foreground'
                    }
                  >
                    {info.online ? '在线' : '离线'}
                  </Badge>
                </div>
                <InfoItem
                  label="屏幕分辨率"
                  value={screen ? `${screen.width} × ${screen.height}` : '未上报'}
                />
                <InfoItem
                  label="assets 分区"
                  value={partitionSize !== null ? formatBytes(partitionSize) : '未上报'}
                />
                <InfoItem label="MCP 控制面" value={describeMcp(info.mcp)} />
                <InfoItem label="client_id" value={info.client_id ?? '—'} mono />
                <InfoItem
                  label="屏幕色深"
                  value={screen ? (screen.monochrome ? '单色' : 'RGB565') : '未上报'}
                />
              </div>
              {info.warning && (
                <Alert className="border-yellow-500/50 bg-yellow-500/10">
                  <AlertTitle className="text-yellow-700 dark:text-yellow-400">设备提示</AlertTitle>
                  <AlertDescription className="text-yellow-700/90 dark:text-yellow-400/90">
                    {info.warning}
                  </AlertDescription>
                </Alert>
              )}
              {!screen && (
                <Alert className="border-yellow-500/50 bg-yellow-500/10">
                  <AlertTitle className="text-yellow-700 dark:text-yellow-400">
                    未获取到屏幕分辨率
                  </AlertTitle>
                  <AlertDescription className="text-yellow-700/90 dark:text-yellow-400/90">
                    主题包需要 display_config，请确认设备在线且固件支持
                    `self.screen.get_info`，然后点击「刷新设备信息」。
                  </AlertDescription>
                </Alert>
              )}
              {screen?.monochrome && (
                <Alert>
                  <AlertTitle>单色屏</AlertTitle>
                  <AlertDescription>
                    该设备为单色屏，图片背景不会被固件使用；建议只使用表情 / 字体 / 字幕配置。
                  </AlertDescription>
                </Alert>
              )}
            </div>
          )}
        </div>

        {/* 背景 */}
        <div className="space-y-2">
          <div>
            <h3 className="text-sm font-medium">聊天背景</h3>
            <p className="text-xs text-muted-foreground">
              纯色背景写入 index.json；图片会按屏幕分辨率 cover 裁剪成 RGB565，并带 28 字节 LVGL
              头写成 background_light.raw / background_dark.raw。
            </p>
          </div>
          <div className="grid gap-3 lg:grid-cols-2">
            <BackgroundEditor
              title="浅色背景"
              idSuffix="light"
              fallbackColor={DEFAULT_LIGHT_BACKGROUND}
              draft={light}
              screen={screen}
              onChange={(patch) => updateBackground('light', patch)}
              onPickFile={(file) => void handleBackgroundFile('light', file)}
              disabled={phase !== 'idle'}
            />
            <BackgroundEditor
              title="深色背景"
              idSuffix="dark"
              fallbackColor={DEFAULT_DARK_BACKGROUND}
              draft={dark}
              screen={screen}
              onChange={(patch) => updateBackground('dark', patch)}
              onPickFile={(file) => void handleBackgroundFile('dark', file)}
              disabled={phase !== 'idle'}
            />
          </div>
        </div>

        {/* 表情 */}
        <div className="space-y-2">
          <div>
            <h3 className="text-sm font-medium">表情包（{EMOJI_NAMES.length} 个槽位）</h3>
            <p className="text-xs text-muted-foreground">
              内置官方 Twemoji / Noto Emoji 预设，也可以逐槽上传
              PNG/GIF/JPG/WEBP；静态图会自动缩放到 64×64 以内，GIF
              原样上传。只要自定义了表情，就必须提供 neutral。
            </p>
          </div>
          <div className="space-y-2 rounded-lg border p-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <span className="text-sm font-medium">官方预设</span>
              <Badge variant="outline" className="text-muted-foreground">
                内置
              </Badge>
            </div>
            <div className="grid gap-2 md:grid-cols-3">
              <button
                type="button"
                onClick={clearEmojiPreset}
                disabled={phase !== 'idle' || presetLoading}
                className={cn(
                  'rounded-lg border p-2 text-left transition-colors',
                  emojiPreset === 'none'
                    ? 'border-primary bg-primary/5'
                    : 'border-input hover:border-primary/50',
                )}
              >
                <div className="text-sm font-medium">保持设备当前表情</div>
                <div className="text-xs text-muted-foreground">不写入 emoji_collection</div>
              </button>
              {EMOJI_PRESETS.map((preset) => (
                <button
                  key={preset.id}
                  type="button"
                  onClick={() => void loadEmojiPreset(preset.id)}
                  disabled={phase !== 'idle' || presetLoading}
                  className={cn(
                    'rounded-lg border p-2 text-left transition-colors',
                    emojiPreset === preset.id
                      ? 'border-primary bg-primary/5'
                      : 'border-input hover:border-primary/50',
                  )}
                >
                  <div className="mb-1 flex gap-1">
                    {EMOJI_NAMES.slice(0, 7).map((name) => (
                      <img
                        key={name}
                        src={presetEmojiUrl(preset, name)}
                        alt=""
                        loading="lazy"
                        className="h-6 w-6 object-contain"
                      />
                    ))}
                  </div>
                  <div className="text-sm font-medium">{preset.name}</div>
                  <div className="text-xs text-muted-foreground">{preset.description}</div>
                </button>
              ))}
            </div>
            {presetLoading && (
              <p className="text-xs text-muted-foreground">正在加载官方表情预设...</p>
            )}
            {presetError && <p className="text-xs text-destructive">{presetError}</p>}
            <p className="text-[11px] text-muted-foreground">
              Twemoji：CC-BY 4.0 · Noto Emoji：OFL-1.1；Noto GIF 需要设备 PSRAM。
            </p>
          </div>
          {neutralMissing && (
            <Alert variant="destructive">
              <AlertTitle>缺少必填的 neutral 表情</AlertTitle>
              <AlertDescription>
                固件在未匹配到表情时会回退到 neutral；已上传自定义表情时请先补上 neutral。
              </AlertDescription>
            </Alert>
          )}
          {hasGif && (
            <Alert className="border-yellow-500/50 bg-yellow-500/10">
              <AlertTitle className="text-yellow-700 dark:text-yellow-400">
                GIF 表情需要 PSRAM
              </AlertTitle>
              <AlertDescription className="text-yellow-700/90 dark:text-yellow-400/90">
                GIF 会原样写入 assets.bin，固件播放动图需要设备有 PSRAM 且固件开启 GIF 支持；没有
                PSRAM 的设备请改用静态 PNG。
              </AlertDescription>
            </Alert>
          )}
          <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-3">
            {EMOJI_NAMES.map((name) => (
              <EmojiSlot
                key={name}
                name={name}
                draft={emojis[name]}
                disabled={phase !== 'idle' || presetLoading}
                onPickFile={(file) => void handleEmojiFile(name, file)}
                onClear={() => clearEmoji(name)}
              />
            ))}
          </div>
        </div>

        {/* 字体与字幕 */}
        <div className="grid gap-3 lg:grid-cols-2">
          <div className="space-y-2 rounded-lg border p-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <span className="text-sm font-medium">字体</span>
              <Badge variant="outline" className="text-muted-foreground">
                {fontData ? '自定义字体' : '保持设备当前字体'}
              </Badge>
            </div>
            <p className="text-xs text-muted-foreground">
              默认不写 text_font；如需更换字体，请上传预编译的 CBin .bin（会以 font_custom.bin
              写入）。不支持 TTF/OTF 转换。
            </p>
            <input
              ref={fontInputRef}
              type="file"
              accept=".bin,application/octet-stream"
              aria-label="自定义字体 .bin 文件"
              className="hidden"
              onChange={(event) => {
                const file = event.target.files?.[0];
                event.target.value = '';
                if (file) void handleFontFile(file);
              }}
            />
            <div className="flex flex-wrap items-center gap-2">
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => fontInputRef.current?.click()}
                disabled={fontConverting || phase !== 'idle'}
              >
                {fontConverting ? '读取中...' : fontData ? '更换字体' : '选择 .bin 字体'}
              </Button>
              {(fontFile || fontData) && (
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  onClick={clearFont}
                  disabled={fontConverting || phase !== 'idle'}
                >
                  恢复默认
                </Button>
              )}
              <span className="min-w-0 truncate text-xs text-muted-foreground">
                {fontFile
                  ? `${fontFile.name} · ${fontData ? formatBytes(fontData.byteLength) : '读取中...'}`
                  : '未选择'}
              </span>
            </div>
            {fontError && <p className="text-xs text-destructive">{fontError}</p>}
          </div>

          <div className="space-y-2 rounded-lg border p-3">
            <span className="text-sm font-medium">底部字幕</span>
            <label className="flex items-start gap-2 rounded-md p-1 hover:bg-muted/40">
              <input
                type="checkbox"
                checked={hideSubtitle}
                onChange={(event) => setHideSubtitle(event.target.checked)}
                className="mt-0.5 h-4 w-4 shrink-0 accent-primary"
              />
              <span className="text-sm">
                隐藏底部对话字幕（hide_subtitle）
                <span className="mt-0.5 block text-xs text-muted-foreground">
                  仅对固件默认的「底部单行字幕」UI 生效；气泡 / 多行字幕由固件编译选项决定， assets
                  无法改动。修改后需重启设备。
                </span>
              </span>
            </label>
          </div>
        </div>

        {/* 主题包与下发 */}
        <div className="space-y-3 rounded-lg border p-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="text-sm font-medium">主题包</span>
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
              <span>大小：{assetsBin ? formatBytes(binSize) : '—'}</span>
              <span>分区：{partitionSize !== null ? formatBytes(partitionSize) : '未上报'}</span>
              <span>文件：{packageEntries.length}</span>
            </div>
          </div>

          {packageEntries.length > 0 ? (
            <Table className="min-w-[24rem]" aria-label="主题包文件列表">
              <TableHeader>
                <TableRow>
                  <TableHead>文件</TableHead>
                  <TableHead className="w-24 text-right">大小</TableHead>
                  <TableHead className="w-24 text-right">尺寸</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {packageEntries.map((entry) => (
                  <TableRow key={entry.name}>
                    <TableCell className="font-mono text-xs">{entry.name}</TableCell>
                    <TableCell className="text-right text-xs">{formatBytes(entry.size)}</TableCell>
                    <TableCell className="text-right text-xs text-muted-foreground">
                      {entry.width && entry.height ? `${entry.width}×${entry.height}` : '—'}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          ) : (
            <p className="text-xs text-muted-foreground">还没有可打包的文件。</p>
          )}

          {nearPartitionLimit && (
            <Alert className="border-yellow-500/50 bg-yellow-500/10">
              <AlertTitle className="text-yellow-700 dark:text-yellow-400">接近分区上限</AlertTitle>
              <AlertDescription className="text-yellow-700/90 dark:text-yellow-400/90">
                主题包已超过 assets 分区的 90%，建议减少表情数量或改用更小的背景。
              </AlertDescription>
            </Alert>
          )}
          {overPartition && (
            <Alert variant="destructive">
              <AlertTitle>主题包超过 assets 分区</AlertTitle>
              <AlertDescription>
                当前 {formatBytes(binSize)}，设备 assets 分区仅 {formatBytes(partitionSize ?? 0)}
                。请减少表情或换用更小的背景；已禁用下发。
              </AlertDescription>
            </Alert>
          )}
          {overUploadLimit && !overPartition && (
            <Alert variant="destructive">
              <AlertTitle>主题包超过上传上限</AlertTitle>
              <AlertDescription>
                后端单次只接受 {formatBytes(MAX_UPLOAD_BYTES)}，请减少表情或换用更小的背景。
              </AlertDescription>
            </Alert>
          )}
          {blockReason && (
            <Alert>
              <AlertTitle>暂不可下发</AlertTitle>
              <AlertDescription>{blockReason}</AlertDescription>
            </Alert>
          )}
          {actionError && (
            <Alert variant="destructive">
              <AlertTitle>操作失败</AlertTitle>
              <AlertDescription>{actionError}</AlertDescription>
            </Alert>
          )}
          {notice && (
            <Alert className="border-green-500/50 bg-green-500/10">
              <AlertTitle className="text-green-700 dark:text-green-400">操作成功</AlertTitle>
              <AlertDescription className="text-green-700/90 dark:text-green-400/90">
                {notice}
                {uploaded?.url && (
                  <p className="mt-1 font-mono text-xs break-all">{uploaded.url}</p>
                )}
              </AlertDescription>
            </Alert>
          )}

          <div className="flex flex-wrap items-center gap-2">
            <Button type="button" onClick={() => void handleApply()} disabled={applyDisabled}>
              {phase === 'uploading'
                ? '上传中...'
                : phase === 'applying'
                  ? '下发中...'
                  : '生成并应用主题'}
            </Button>
            <span className="text-xs text-muted-foreground">
              上传后通过 MCP 下发下载地址并重启设备；设备将自动拉取并应用 assets.bin。
            </span>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}
