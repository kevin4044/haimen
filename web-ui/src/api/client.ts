const API_BASE = '';

export class ApiError extends Error {
  code: string;
  status: number;

  constructor(code: string, message: string, status: number) {
    super(message);
    this.name = 'ApiError';
    this.code = code;
    this.status = status;
  }
}

export async function apiFetch<T>(path: string, options: RequestInit = {}): Promise<T> {
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    ...(options.headers as Record<string, string>),
  };

  const res = await fetch(`${API_BASE}${path}`, { ...options, headers });

  if (!res.ok) {
    const raw = await res.json().catch(() => ({}));
    const body = raw && typeof raw === 'object' ? (raw as Record<string, unknown>) : {};
    const message =
      (typeof body.message === 'string' && body.message) ||
      (typeof body.error === 'string' && body.error) ||
      (typeof body.detail === 'string' && body.detail) ||
      res.statusText;
    throw new ApiError(typeof body.code === 'string' ? body.code : 'UNKNOWN', message, res.status);
  }

  return res.json() as Promise<T>;
}
