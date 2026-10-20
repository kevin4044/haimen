import type { ApiResponse, XiaozhiConfiguredDevice, XiaozhiDevicesData } from '@/types';
import { apiFetch } from './client';

function ensureData<T>(res: ApiResponse<T>): T {
  if (res.data == null) throw new Error('Empty response');
  return res.data;
}

/** 保存设备表时提交的单台设备（device_id 支持 MAC 或 MQTT client_id） */
export interface XiaozhiDeviceInput {
  device_id: string;
  name: string;
  room: string;
}

/** 获取已配置设备与当前在线设备 */
export async function getXiaozhiDevices(): Promise<XiaozhiDevicesData> {
  const res = await apiFetch<ApiResponse<XiaozhiDevicesData>>('/api/v1/settings/xiaozhi/devices');
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
