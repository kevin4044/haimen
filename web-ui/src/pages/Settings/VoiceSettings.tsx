import { Eye, EyeOff } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import {
  getAsrSettings,
  getTtsSettings,
  listTtsVoices,
  updateAsrSettings,
  updateTtsSettings,
  verifyAsrCredentials,
  verifyTtsCredentials,
} from '@/api/voice';
import Combobox from '@/components/Combobox';
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { Switch } from '@/components/ui/switch';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import VoiceSelector from '@/components/VoiceSelector';
import { ASR_PROVIDERS } from '@/data/asr-providers';
import { TTS_PROVIDERS } from '@/data/tts-providers';
import type { AsrSettings, TtsSettings, TtsVoice } from '@/types';

function PasswordInput({
  id,
  value,
  placeholder,
  onChange,
}: {
  id: string;
  value: string;
  placeholder: string;
  onChange: (v: string) => void;
}) {
  const [show, setShow] = useState(false);
  return (
    <div className="relative">
      <Input
        id={id}
        type={show ? 'text' : 'password'}
        placeholder={placeholder}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="pr-10"
      />
      <button
        type="button"
        aria-label={show ? '隐藏' : '显示'}
        onClick={() => setShow(!show)}
        tabIndex={-1}
        className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground cursor-pointer select-none"
      >
        {show ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
      </button>
    </div>
  );
}

/** 配置中缺少该字段、但后端已解析出回退值时的提示（不展示具体值） */
function ResolvedHint({ show, value }: { show: boolean; value: string | null | undefined }) {
  if (!show || !value) return null;
  return <p className="text-xs text-muted-foreground">当前生效值来自环境变量/默认值</p>;
}

// ─── ASR 配置区域 ───────────────────────────────────────────

function AsrSettingsPanel() {
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [editState, setEditState] = useState<Record<string, Record<string, string>>>({});
  const [activeProvider, setActiveProvider] = useState('doubao');
  const [selectedTab, setSelectedTab] = useState('doubao');
  const [saving, setSaving] = useState(false);
  const [saveResult, setSaveResult] = useState<string | null>(null);
  const [verifying, setVerifying] = useState(false);
  const [verifyResult, setVerifyResult] = useState<{ valid: boolean; message: string } | null>(
    null,
  );
  const [resolved, setResolved] = useState<AsrSettings['resolved']>(undefined);
  const [providerWarning, setProviderWarning] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await getAsrSettings();
      const active = data.active_provider ?? 'doubao';
      const known = ASR_PROVIDERS.some((p) => p.id === active);
      const fallback = ASR_PROVIDERS[0]?.id ?? 'doubao';
      const fallbackName = ASR_PROVIDERS.find((p) => p.id === fallback)?.name ?? fallback;
      setEditState(data.providers ?? {});
      setResolved(data.resolved);
      setActiveProvider(active);
      setSelectedTab(known ? active : fallback);
      setProviderWarning(
        known
          ? null
          : `当前激活服务商「${active}」不在支持列表中，已切换到「${fallbackName}」标签页，请重新选择后再保存。`,
      );
      setLoading(false);
    } catch {
      setError('加载 ASR 配置失败');
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const handleFieldChange = (key: string, value: string) => {
    setEditState((prev) => ({
      ...prev,
      [selectedTab]: {
        ...(prev[selectedTab] ?? {}),
        [key]: value,
      },
    }));
  };

  /** 重新加载 ASR 配置数据，但不重置当前选中的 Tab */
  const reloadAsrData = useCallback(async () => {
    try {
      const data = await getAsrSettings();
      setEditState(data.providers ?? {});
      setResolved(data.resolved);
      setActiveProvider(data.active_provider ?? 'doubao');
    } catch {
      // 静默失败
    }
  }, []);

  const handleSave = async () => {
    setSaving(true);
    try {
      await updateAsrSettings({
        active_provider: activeProvider,
        providers: editState,
      });
      setSaveResult('保存成功');
      setVerifyResult(null);
      await reloadAsrData();
    } catch {
      setSaveResult('保存失败');
    } finally {
      setSaving(false);
      setTimeout(() => setSaveResult(null), 3000);
    }
  };

  const handleSetActive = async () => {
    setSaving(true);
    try {
      await updateAsrSettings({
        active_provider: selectedTab,
        providers: editState,
      });
      setActiveProvider(selectedTab);
      setProviderWarning(null);
      setSaveResult('已切换激活服务商');
      await reloadAsrData();
    } catch {
      setSaveResult('保存失败');
    } finally {
      setSaving(false);
      setTimeout(() => setSaveResult(null), 3000);
    }
  };

  const handleVerify = async () => {
    const creds = editState[selectedTab] ?? {};
    if (!Object.values(creds).some((v) => v.length > 0)) return;

    setVerifying(true);
    try {
      const result = await verifyAsrCredentials(creds, selectedTab);
      setVerifyResult(result);
    } catch {
      setVerifyResult({ valid: false, message: '网络请求失败' });
    } finally {
      setVerifying(false);
    }
  };

  if (loading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>ASR 语音识别</CardTitle>
        </CardHeader>
        <CardContent>
          <Skeleton className="h-48 w-full" />
        </CardContent>
      </Card>
    );
  }

  if (error) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>ASR 语音识别</CardTitle>
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

  const resolvedMap = resolved as Record<string, string | null> | undefined;

  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="flex items-center gap-2 text-base">ASR 语音识别</CardTitle>
      </CardHeader>
      <CardContent>
        {providerWarning && (
          <Alert className="mb-4">
            <AlertTitle>激活服务商不可用</AlertTitle>
            <AlertDescription>{providerWarning}</AlertDescription>
          </Alert>
        )}
        <Tabs value={selectedTab} onValueChange={setSelectedTab} className="w-full">
          {/* Tab 栏 */}
          <TabsList>
            {ASR_PROVIDERS.map((p) => {
              const isActive = p.id === activeProvider;

              return (
                <TabsTrigger key={p.id} value={p.id}>
                  {p.name}
                  {isActive && (
                    <Badge variant="default" className="text-[10px] px-1.5 py-0 leading-4">
                      ✓
                    </Badge>
                  )}
                </TabsTrigger>
              );
            })}
          </TabsList>

          {/* 每个服务商的配置面板 */}
          {ASR_PROVIDERS.map((p) => (
            <TabsContent key={p.id} value={p.id} className="space-y-4 mt-4">
              {/* 服务商名称和状态 */}
              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <span className="font-medium text-foreground">{p.name}</span>
                {p.id === activeProvider ? (
                  <Badge variant="default" className="text-[10px] px-1.5">
                    当前激活
                  </Badge>
                ) : (
                  <Badge variant="outline" className="text-[10px] px-1.5">
                    未激活
                  </Badge>
                )}
              </div>

              {/* 动态字段 */}
              {p.fields.map((field) => (
                <div key={field.key} className="space-y-2">
                  <span className="text-sm font-medium">{field.label}</span>
                  {field.type === 'select' && field.options ? (
                    <Combobox
                      options={field.options.map((opt) => ({ value: opt, label: opt }))}
                      value={editState[p.id]?.[field.key] ?? null}
                      onChange={(v) => handleFieldChange(field.key, v)}
                      placeholder={field.placeholder ?? '请选择...'}
                    />
                  ) : field.type === 'number' || field.type === 'text' ? (
                    <Input
                      id={`asr-${p.id}-${field.key}`}
                      type={field.type}
                      placeholder={field.placeholder ?? `输入${field.label}`}
                      value={editState[p.id]?.[field.key] ?? ''}
                      onChange={(e) => handleFieldChange(field.key, e.target.value)}
                    />
                  ) : (
                    <PasswordInput
                      id={`asr-${p.id}-${field.key}`}
                      placeholder={field.placeholder ?? `输入${field.label}`}
                      value={editState[p.id]?.[field.key] ?? ''}
                      onChange={(v) => handleFieldChange(field.key, v)}
                    />
                  )}
                  <ResolvedHint
                    show={!editState[p.id]?.[field.key] && p.id === activeProvider}
                    value={resolvedMap?.[field.key]}
                  />
                </div>
              ))}

              {/* 验证结果 */}
              {verifyResult && (
                <Alert variant={verifyResult.valid ? 'default' : 'destructive'}>
                  <AlertTitle>{verifyResult.valid ? '验证通过' : '验证失败'}</AlertTitle>
                  <AlertDescription>{verifyResult.message}</AlertDescription>
                </Alert>
              )}

              {/* 操作按钮 */}
              <div className="flex items-center gap-2 pt-2 flex-wrap">
                <Button
                  variant="outline"
                  onClick={handleVerify}
                  disabled={verifying || !p.fields.some((f) => editState[p.id]?.[f.key])}
                >
                  {verifying ? '验证中...' : '验证凭证'}
                </Button>
                <Button onClick={handleSave} disabled={saving}>
                  {saving ? '保存中...' : '保存配置'}
                </Button>
                {p.id !== activeProvider && (
                  <Button variant="secondary" onClick={handleSetActive} disabled={saving}>
                    {saving ? '保存中...' : '🌟 设为首选'}
                  </Button>
                )}
                {saveResult && (
                  <span
                    className={`text-sm ${
                      saveResult === '保存成功' || saveResult === '已切换激活服务商'
                        ? 'text-green-600'
                        : 'text-red-600'
                    }`}
                  >
                    {saveResult}
                  </span>
                )}
              </div>
            </TabsContent>
          ))}
        </Tabs>
      </CardContent>
    </Card>
  );
}

// ─── TTS 配置区域 ───────────────────────────────────────────

function TtsSettingsPanel() {
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [editState, setEditState] = useState<Record<string, Record<string, string>>>({});
  const [fixedTextEnabled, setFixedTextEnabled] = useState(false);
  const [fixedText, setFixedText] = useState('');
  const [wakeGreetingEnabled, setWakeGreetingEnabled] = useState(true);
  const [wakeGreeting, setWakeGreeting] = useState('');
  const [noSpeechGoodbye, setNoSpeechGoodbye] = useState('');
  const [noSpeechTimeoutSecs, setNoSpeechTimeoutSecs] = useState(10);
  const [thinkingFeedbackEnabled, setThinkingFeedbackEnabled] = useState(true);
  const [thinkingFeedbackIntervalSecs, setThinkingFeedbackIntervalSecs] = useState(15);
  const [thinkingFeedbackText, setThinkingFeedbackText] = useState('');
  const [thinkingFeedbackTimeoutText, setThinkingFeedbackTimeoutText] = useState('');
  const [activeProvider, setActiveProvider] = useState('doubao');
  const [selectedTab, setSelectedTab] = useState('doubao');
  const [voices, setVoices] = useState<TtsVoice[]>([]);
  const [saving, setSaving] = useState(false);
  const [saveResult, setSaveResult] = useState<string | null>(null);
  const [verifying, setVerifying] = useState(false);
  const [verifyResult, setVerifyResult] = useState<{ valid: boolean; message: string } | null>(
    null,
  );
  const [resolved, setResolved] = useState<TtsSettings['resolved']>(undefined);
  const [providerWarning, setProviderWarning] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await getTtsSettings();
      const active = data.active_provider ?? 'doubao';
      const known = TTS_PROVIDERS.some((p) => p.id === active);
      const fallback = TTS_PROVIDERS[0]?.id ?? 'doubao';
      const fallbackName = TTS_PROVIDERS.find((p) => p.id === fallback)?.name ?? fallback;
      setEditState(data.providers ?? {});
      setFixedTextEnabled(data.fixed_text_enabled ?? false);
      setFixedText(data.fixed_text ?? '');
      setWakeGreetingEnabled(data.wake_greeting_enabled ?? true);
      setWakeGreeting(data.wake_greeting ?? '');
      setNoSpeechGoodbye(data.no_speech_goodbye ?? '');
      setNoSpeechTimeoutSecs((data.no_speech_timeout_ms ?? 10000) / 1000);
      setThinkingFeedbackEnabled(data.thinking_feedback_enabled ?? true);
      setThinkingFeedbackIntervalSecs((data.thinking_feedback_interval_ms ?? 15000) / 1000);
      setThinkingFeedbackText(data.thinking_feedback_text ?? '');
      setThinkingFeedbackTimeoutText(data.thinking_feedback_timeout_text ?? '');
      setResolved(data.resolved);
      setActiveProvider(active);
      setSelectedTab(known ? active : fallback);
      setProviderWarning(
        known
          ? null
          : `当前激活服务商「${active}」不在支持列表中，已切换到「${fallbackName}」标签页，请重新选择后再保存。`,
      );
      // 音色列表由下方 effect（依赖 selectedTab / currentModel）加载
    } catch {
      setError('加载 TTS 配置失败');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  // 切换 Tab 时加载对应提供商的音色。
  // 豆包返回全部音色（VoiceSelector 按所属模型分组展示），
  // 选择音色时自动设置对应的 resource_id，无需用户手动切换模型。
  useEffect(() => {
    let cancelled = false;
    // 先清空，避免切换 Tab 时短暂沿用上一个提供商的音色列表
    //（如 openai 没有音色列表，不能复用豆包的选择器）。
    setVoices([]);
    listTtsVoices(selectedTab)
      .then((list) => {
        if (cancelled) return;
        setVoices(list);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [selectedTab]);

  const handleFieldChange = (key: string, value: string) => {
    setEditState((prev) => ({
      ...prev,
      [selectedTab]: {
        ...(prev[selectedTab] ?? {}),
        [key]: value,
      },
    }));
  };

  /** 重新加载配置数据，但不重置当前选中的 Tab */
  const reloadData = useCallback(async () => {
    try {
      const data = await getTtsSettings();
      setEditState(data.providers ?? {});
      setResolved(data.resolved);
      setActiveProvider(data.active_provider ?? 'doubao');
    } catch {
      // 静默失败，保持已有状态
    }
  }, []);

  const handleSave = async () => {
    setSaving(true);
    try {
      await updateTtsSettings({
        active_provider: activeProvider,
        providers: editState,
        fixed_text_enabled: fixedTextEnabled,
        fixed_text: fixedText || null,
        wake_greeting_enabled: wakeGreetingEnabled,
        wake_greeting: wakeGreeting || null,
        no_speech_goodbye: noSpeechGoodbye || null,
        no_speech_timeout_ms: noSpeechTimeoutSecs * 1000,
        thinking_feedback_enabled: thinkingFeedbackEnabled,
        thinking_feedback_interval_ms: Math.round(thinkingFeedbackIntervalSecs * 1000),
        thinking_feedback_text: thinkingFeedbackText || null,
        thinking_feedback_timeout_text: thinkingFeedbackTimeoutText || null,
      });
      setSaveResult('保存成功');
      setVerifyResult(null);
      await reloadData();
    } catch {
      setSaveResult('保存失败');
    } finally {
      setSaving(false);
      setTimeout(() => setSaveResult(null), 3000);
    }
  };

  const handleSetActive = async () => {
    setSaving(true);
    try {
      await updateTtsSettings({
        active_provider: selectedTab,
        providers: editState,
        fixed_text_enabled: fixedTextEnabled,
        fixed_text: fixedText || null,
        wake_greeting_enabled: wakeGreetingEnabled,
        wake_greeting: wakeGreeting || null,
        no_speech_goodbye: noSpeechGoodbye || null,
        no_speech_timeout_ms: noSpeechTimeoutSecs * 1000,
        thinking_feedback_enabled: thinkingFeedbackEnabled,
        thinking_feedback_interval_ms: Math.round(thinkingFeedbackIntervalSecs * 1000),
        thinking_feedback_text: thinkingFeedbackText || null,
        thinking_feedback_timeout_text: thinkingFeedbackTimeoutText || null,
      });
      setActiveProvider(selectedTab);
      setProviderWarning(null);
      setSaveResult('已切换激活服务商');
      await reloadData();
    } catch {
      setSaveResult('保存失败');
    } finally {
      setSaving(false);
      setTimeout(() => setSaveResult(null), 3000);
    }
  };

  const handleVerify = async () => {
    const creds = editState[selectedTab] ?? {};
    if (!Object.values(creds).some((v) => v.length > 0)) return;

    setVerifying(true);
    try {
      const result = await verifyTtsCredentials(creds, selectedTab);
      setVerifyResult(result);
    } catch {
      setVerifyResult({ valid: false, message: '网络请求失败' });
    } finally {
      setVerifying(false);
    }
  };

  if (loading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>TTS 语音合成</CardTitle>
        </CardHeader>
        <CardContent>
          <Skeleton className="h-48 w-full" />
        </CardContent>
      </Card>
    );
  }

  if (error) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>TTS 语音合成</CardTitle>
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

  const resolvedMap = resolved as Record<string, string | null> | undefined;

  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="flex items-center gap-2 text-base">TTS 语音合成</CardTitle>
      </CardHeader>
      <CardContent>
        {providerWarning && (
          <Alert className="mb-4">
            <AlertTitle>激活服务商不可用</AlertTitle>
            <AlertDescription>{providerWarning}</AlertDescription>
          </Alert>
        )}
        <Tabs value={selectedTab} onValueChange={setSelectedTab} className="w-full">
          {/* Tab 栏 */}
          <TabsList>
            {TTS_PROVIDERS.map((p) => {
              const isActive = p.id === activeProvider;

              return (
                <TabsTrigger key={p.id} value={p.id}>
                  {p.name}
                  {isActive && (
                    <Badge variant="default" className="text-[10px] px-1.5 py-0 leading-4">
                      ✓
                    </Badge>
                  )}
                </TabsTrigger>
              );
            })}
          </TabsList>

          {/* 每个服务商的配置面板 */}
          {TTS_PROVIDERS.map((p) => (
            <TabsContent key={p.id} value={p.id} className="space-y-4 mt-4">
              {/* 服务商名称和状态 */}
              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <span className="font-medium text-foreground">{p.name}</span>
                {p.id === activeProvider ? (
                  <Badge variant="default" className="text-[10px] px-1.5">
                    当前激活
                  </Badge>
                ) : (
                  <Badge variant="outline" className="text-[10px] px-1.5">
                    未激活
                  </Badge>
                )}
              </div>

              {/* 动态字段 */}
              {p.fields.map((field) => (
                <div key={field.key} className="space-y-2">
                  <span className="text-sm font-medium">{field.label}</span>
                  {field.type === 'select' && field.options ? (
                    <Combobox
                      options={field.options.map((opt) => ({ value: opt, label: opt }))}
                      value={editState[p.id]?.[field.key] ?? null}
                      onChange={(v) => handleFieldChange(field.key, v)}
                      placeholder={field.placeholder ?? '请选择...'}
                    />
                  ) : field.type === 'text' ? (
                    <Input
                      id={`tts-${p.id}-${field.key}`}
                      type="text"
                      placeholder={field.placeholder ?? `输入${field.label}`}
                      value={editState[p.id]?.[field.key] ?? ''}
                      onChange={(e) => handleFieldChange(field.key, e.target.value)}
                    />
                  ) : field.type === 'number' ? (
                    <Input
                      id={`tts-${p.id}-${field.key}`}
                      type="number"
                      min={field.min}
                      max={field.max}
                      step={field.step}
                      placeholder={field.placeholder ?? `输入${field.label}`}
                      value={editState[p.id]?.[field.key] ?? ''}
                      onChange={(e) => handleFieldChange(field.key, e.target.value)}
                    />
                  ) : (
                    <PasswordInput
                      id={`tts-${p.id}-${field.key}`}
                      placeholder={field.placeholder ?? `输入${field.label}`}
                      value={editState[p.id]?.[field.key] ?? ''}
                      onChange={(v) => handleFieldChange(field.key, v)}
                    />
                  )}
                  <ResolvedHint
                    show={!editState[p.id]?.[field.key] && p.id === activeProvider}
                    value={resolvedMap?.[field.key]}
                  />
                </div>
              ))}

              {/* 音色选择器（仅当前选中 Tab 展示） */}
              {p.id === selectedTab && voices.length > 0 && (
                <div className="space-y-2">
                  <span className="text-sm font-medium" id="tts-voice-label">
                    TTS 音色
                  </span>
                  <div className="flex items-start gap-2">
                    <div className="flex-1">
                      <VoiceSelector
                        voices={voices}
                        selectedVoice={editState[p.id]?.voice ?? null}
                        onChange={(voiceId) => {
                          handleFieldChange('voice', voiceId);
                          // 音色决定模型：选择音色时自动把 resource_id 设为该音色所属模型
                          const voice = voices.find((v) => v.id === voiceId);
                          if (voice?.model) {
                            handleFieldChange('resource_id', voice.model);
                          }
                        }}
                      />
                    </div>
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      onClick={() => handleFieldChange('voice', '')}
                    >
                      使用默认音色
                    </Button>
                  </div>
                  <ResolvedHint
                    show={!editState[p.id]?.voice && p.id === activeProvider}
                    value={resolved?.voice}
                  />
                </div>
              )}

              {/* 验证结果 */}
              {verifyResult && (
                <Alert variant={verifyResult.valid ? 'default' : 'destructive'}>
                  <AlertTitle>{verifyResult.valid ? '验证通过' : '验证失败'}</AlertTitle>
                  <AlertDescription>{verifyResult.message}</AlertDescription>
                </Alert>
              )}

              {/* 操作按钮 */}
              <div className="flex items-center gap-2 pt-2 flex-wrap">
                <Button
                  variant="outline"
                  onClick={handleVerify}
                  disabled={verifying || !p.fields.some((f) => editState[p.id]?.[f.key])}
                >
                  {verifying ? '验证中...' : '验证凭证'}
                </Button>
                <Button onClick={handleSave} disabled={saving}>
                  {saving ? '保存中...' : '保存配置'}
                </Button>
                {p.id !== activeProvider && (
                  <Button variant="secondary" onClick={handleSetActive} disabled={saving}>
                    {saving ? '保存中...' : '🌟 设为首选'}
                  </Button>
                )}
                {saveResult && (
                  <span
                    className={`text-sm ${
                      saveResult === '保存成功' || saveResult === '已切换激活服务商'
                        ? 'text-green-600'
                        : 'text-red-600'
                    }`}
                  >
                    {saveResult}
                  </span>
                )}
              </div>
            </TabsContent>
          ))}
        </Tabs>

        {/* ── 固定文本模式 ── */}
        <div className="mt-6 rounded-lg border p-4 space-y-3">
          <div className="flex items-center justify-between">
            <div className="space-y-0.5">
              <span className="text-sm font-medium">固定文本模式</span>
              <p className="text-xs text-muted-foreground">
                开启后，无论 ASR 识别到什么内容，设备都将播放下方指定的文本
              </p>
            </div>
            <Switch checked={fixedTextEnabled} onCheckedChange={setFixedTextEnabled} />
          </div>
          {fixedTextEnabled && (
            <Input
              placeholder="输入要播放的固定文本..."
              value={fixedText}
              onChange={(e) => setFixedText(e.target.value)}
            />
          )}
        </div>

        {/* ── 交互语设置 ── */}
        <div className="mt-6 rounded-lg border p-4 space-y-4">
          <div className="space-y-0.5">
            <span className="text-sm font-medium">交互语</span>
            <p className="text-xs text-muted-foreground">
              自定义设备唤醒问候与无语音超时的告别语（留空回退默认文案）
            </p>
          </div>

          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-sm font-medium">唤醒问候</span>
              <Switch checked={wakeGreetingEnabled} onCheckedChange={setWakeGreetingEnabled} />
            </div>
            {wakeGreetingEnabled && (
              <Input
                placeholder="唤醒时播报的问候语，留空为「你好」"
                value={wakeGreeting}
                onChange={(e) => setWakeGreeting(e.target.value)}
              />
            )}
          </div>

          <div className="space-y-2">
            <span className="text-sm font-medium">无语音告别</span>
            <p className="text-xs text-muted-foreground">
              设备唤醒后若持续不说话，超时后播报该告别语并结束对话
            </p>
            <Input
              placeholder="超时告别语，留空为「拜拜」"
              value={noSpeechGoodbye}
              onChange={(e) => setNoSpeechGoodbye(e.target.value)}
            />
            <div className="flex items-center gap-2">
              <Input
                type="number"
                min={0}
                placeholder="超时秒数"
                value={noSpeechTimeoutSecs}
                onChange={(e) => setNoSpeechTimeoutSecs(Number(e.target.value) || 0)}
                className="w-28"
              />
              <span className="text-sm text-muted-foreground">秒无语音后触发（0 禁用）</span>
            </div>
          </div>
        </div>

        {/* ── 处理进度提示 ── */}
        <div className="mt-6 rounded-lg border p-4 space-y-4">
          <div className="flex items-center justify-between">
            <div className="space-y-0.5">
              <span className="text-sm font-medium">处理进度提示</span>
              <p className="text-xs text-muted-foreground">
                等待 Agent 首个可播文本期间按间隔播报提示语；间隔为 0 时仅在等待超时后兜底播报
              </p>
            </div>
            <Switch
              checked={thinkingFeedbackEnabled}
              onCheckedChange={setThinkingFeedbackEnabled}
            />
          </div>

          <div className="space-y-2">
            <span className="text-sm font-medium">提示间隔（秒）</span>
            <div className="flex items-center gap-2">
              <Input
                type="number"
                min={0}
                step={1}
                placeholder="15"
                value={thinkingFeedbackIntervalSecs}
                onChange={(e) => setThinkingFeedbackIntervalSecs(Number(e.target.value) || 0)}
                className="w-28"
              />
              <span className="text-xs text-muted-foreground">0 表示仅在等待超时后兜底提示</span>
            </div>
          </div>

          <div className="space-y-2">
            <span className="text-sm font-medium">周期提示文案</span>
            <Input
              placeholder="等待时的周期提示，留空用默认"
              value={thinkingFeedbackText}
              onChange={(e) => setThinkingFeedbackText(e.target.value)}
            />
          </div>

          <div className="space-y-2">
            <span className="text-sm font-medium">超时兜底文案</span>
            <Input
              placeholder="超时兜底文案，留空用默认"
              value={thinkingFeedbackTimeoutText}
              onChange={(e) => setThinkingFeedbackTimeoutText(e.target.value)}
            />
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ─── 页面 ───────────────────────────────────────────────────

export default function VoiceSettings() {
  return (
    <div className="mx-auto max-w-2xl space-y-8 py-8 px-4">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">语音配置</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          配置语音识别 (ASR) 和语音合成 (TTS) 的提供商和参数。 清空字段将回退到环境变量。
        </p>
      </div>
      <AsrSettingsPanel />
      <TtsSettingsPanel />
    </div>
  );
}
