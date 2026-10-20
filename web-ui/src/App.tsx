import { useState } from 'react';
import AgentLogs from '@/pages/AgentLogs';
import RuntimeLogs from '@/pages/RuntimeLogs';
import AgentSettings from '@/pages/Settings/AgentSettings';
import ConnectorSettings from '@/pages/Settings/ConnectorSettings';
import DeviceSettings from '@/pages/Settings/DeviceSettings';
import VoiceSettings from '@/pages/Settings/VoiceSettings';
import XiaozhiSettings from '@/pages/Settings/XiaozhiSettings';

type Page = 'voice' | 'agent' | 'connectors' | 'devices' | 'xiaozhi' | 'runtime-logs' | 'logs';

export default function App() {
  const [page, setPage] = useState<Page>('voice');

  return (
    <div className="min-h-screen bg-background">
      <nav className="border-b px-4 py-2">
        <div className="mx-auto max-w-4xl flex flex-wrap items-center gap-3">
          <h1 className="text-lg font-bold">Haimen</h1>
          <button
            type="button"
            onClick={() => setPage('voice')}
            className={`px-3 py-1 rounded text-sm cursor-pointer ${
              page === 'voice' ? 'bg-primary text-primary-foreground' : 'hover:bg-muted'
            }`}
          >
            语音配置
          </button>
          <button
            type="button"
            onClick={() => setPage('agent')}
            className={`px-3 py-1 rounded text-sm cursor-pointer ${
              page === 'agent' ? 'bg-primary text-primary-foreground' : 'hover:bg-muted'
            }`}
          >
            Agent 配置
          </button>
          <button
            type="button"
            onClick={() => setPage('connectors')}
            className={`px-3 py-1 rounded text-sm cursor-pointer ${
              page === 'connectors' ? 'bg-primary text-primary-foreground' : 'hover:bg-muted'
            }`}
          >
            消息渠道
          </button>
          <button
            type="button"
            onClick={() => setPage('devices')}
            className={`px-3 py-1 rounded text-sm cursor-pointer ${
              page === 'devices' ? 'bg-primary text-primary-foreground' : 'hover:bg-muted'
            }`}
          >
            小智设备
          </button>
          <button
            type="button"
            onClick={() => setPage('xiaozhi')}
            className={`px-3 py-1 rounded text-sm cursor-pointer ${
              page === 'xiaozhi' ? 'bg-primary text-primary-foreground' : 'hover:bg-muted'
            }`}
          >
            小智配置
          </button>
          <button
            type="button"
            onClick={() => setPage('runtime-logs')}
            className={`px-3 py-1 rounded text-sm cursor-pointer ${
              page === 'runtime-logs' ? 'bg-primary text-primary-foreground' : 'hover:bg-muted'
            }`}
          >
            运行日志
          </button>
          <button
            type="button"
            onClick={() => setPage('logs')}
            className={`px-3 py-1 rounded text-sm cursor-pointer ${
              page === 'logs' ? 'bg-primary text-primary-foreground' : 'hover:bg-muted'
            }`}
          >
            对话记录
          </button>
        </div>
      </nav>
      {page === 'voice' && <VoiceSettings />}
      {page === 'agent' && <AgentSettings />}
      {page === 'connectors' && <ConnectorSettings />}
      {page === 'devices' && <DeviceSettings />}
      {page === 'xiaozhi' && <XiaozhiSettings />}
      {page === 'runtime-logs' && <RuntimeLogs />}
      {page === 'logs' && <AgentLogs />}
    </div>
  );
}
