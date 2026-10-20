//! 运行时日志流（SSE）
//!
//! 路由为 `GET /api/v1/logs/stream`（由 `src/web/mod.rs` 挂载）。
//! 将 `~/.haimen/logs/app.log` 作为实时日志源推送给 Web UI：
//!
//! - 连接建立后先发 `event: meta`，数据为 `{"path":"...","tail":200}`；
//! - 随后发 `event: log`，数据为 `{"lines":["原始行", ...]}`；
//! - 先回放最近 `tail` 行，再每 250ms 增量读取新增字节。
//!
//! 后端不解析日志级别，只搬运原始行，级别着色/过滤由前端完成。
//! 流的生命周期与连接绑定（客户端断开 → stream 被 drop），不会遗留后台任务。

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::Query;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::StreamExt;
use futures_util::stream::{self, Stream};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// 首屏回放行数默认值
const DEFAULT_TAIL_LINES: usize = 200;

/// 首屏回放行数上限
const MAX_TAIL_LINES: usize = 1000;

/// 首屏最多回读的字节数（256 KiB），避免大日志文件拖慢首个响应
const MAX_TAIL_BYTES: u64 = 256 * 1024;

/// 增量读取单次上限（64 KiB）
const MAX_READ_BYTES: usize = 64 * 1024;

/// 单条 SSE 事件最多携带的行数
const MAX_LINES_PER_EVENT: usize = 500;

/// 文件轮询间隔
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// 解析 `tail` 查询参数
///
/// 缺省或非法值回退到 200，合法值钳制在 `0..=1000`。
/// `tail=0` 表示不回放历史，只推送连接之后的新行。
fn parse_tail(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_TAIL_LINES)
        .min(MAX_TAIL_LINES)
}

/// 增量读取状态
#[derive(Debug, Default)]
struct ReadState {
    /// 已经从文件读到的字节偏移（下次从这里继续读）
    offset: u64,
    /// 还没等到换行的半行缓冲，留到下一轮拼接
    partial: Vec<u8>,
}

/// 首屏回放结果
#[derive(Debug)]
struct InitialTail {
    /// 首个事件里的完整行
    lines: Vec<String>,
    /// 后续增量读取的起始状态
    read: ReadState,
}

/// 流状态：路径、读取进度、待发送事件队列
struct StreamState {
    path: PathBuf,
    read: ReadState,
    pending: VecDeque<Event>,
}

/// 当前文件长度小于已读偏移 → 文件被截断或轮转，需要从头重读。
fn is_truncated(file_len: u64, offset: u64) -> bool {
    file_len < offset
}

/// 取字节窗口的最后 `max_bytes` 字节。
#[cfg(test)]
fn tail_window(data: &[u8], max_bytes: usize) -> &[u8] {
    &data[data.len().saturating_sub(max_bytes)..]
}

/// 从字节窗口里切出最后 `tail` 行（保留行尾换行）。
///
/// 行数按 `\n` 计算，末尾没有换行的残行也算一行，与 `str::lines` 的计数一致。
/// `tail == 0` 返回空切片；窗口内行数不足时返回整个窗口。
fn tail_slice(data: &[u8], tail: usize) -> &[u8] {
    if tail == 0 || data.is_empty() {
        return &[];
    }

    // 末尾换行不额外产生空行，计数时忽略最后一个字节
    let effective_end = if data.last() == Some(&b'\n') {
        data.len() - 1
    } else {
        data.len()
    };
    let total_lines = data[..effective_end]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    if tail >= total_lines {
        return data;
    }

    // 跳过开头的 total_lines - tail 个换行，落在目标首行的行首
    let skip = total_lines - tail;
    let mut seen = 0;
    for (index, byte) in data.iter().enumerate() {
        if *byte == b'\n' {
            seen += 1;
            if seen == skip {
                return &data[index + 1..];
            }
        }
    }
    data
}

/// 解码一行原始字节
///
/// 去掉行尾 `\r`（兼容 CRLF），其余内容原样保留；非法 UTF-8 用替换字符兜底，
/// 避免一条坏日志打断整个流。
fn decode_line(raw: &[u8]) -> String {
    let raw = if raw.last() == Some(&b'\r') {
        &raw[..raw.len() - 1]
    } else {
        raw
    };
    String::from_utf8_lossy(raw).into_owned()
}

/// 把字节缓冲切成完整行，并把没有换行结尾的尾巴留在 `rest` 里。
///
/// 最多返回 `max_lines` 行；触达上限时，剩余字节（包括后续完整行）全部放进
/// `rest` 等下一轮处理，确保既不超过单事件行数上限，也不丢日志。
fn split_complete_lines(buf: &[u8], max_lines: usize) -> (Vec<String>, Vec<u8>) {
    let mut lines = Vec::new();
    let mut line_start = 0;
    let mut consumed = 0;

    while lines.len() < max_lines {
        let Some(position) = buf[line_start..].iter().position(|byte| *byte == b'\n') else {
            break;
        };
        let newline = line_start + position;
        lines.push(decode_line(&buf[line_start..newline]));
        line_start = newline + 1;
        consumed = line_start;
    }

    (lines, buf[consumed..].to_vec())
}

/// 把一次读到的字节并入半行缓冲，返回本批可发送的完整行。
///
/// `offset` 始终推进到本次读取的末尾；尚未成行的字节留在 `partial` 里，
/// 下一轮与新字节拼接后再切分。
fn consume_chunk(state: &mut ReadState, chunk: &[u8], max_lines: usize) -> Vec<String> {
    let mut buf = std::mem::take(&mut state.partial);
    buf.extend_from_slice(chunk);
    state.offset += chunk.len() as u64;

    let (lines, rest) = split_complete_lines(&buf, max_lines);
    state.partial = rest;
    lines
}

/// 读取首屏 `tail` 行
///
/// 最多回读文件末尾 256 KiB；窗口从文件中间开始时，第一行大概率被截断，
/// 直接丢弃到第一个换行之后。文件不存在或读取失败返回空结果，让流继续等待。
fn read_initial_tail(path: &Path, tail: usize) -> InitialTail {
    use std::io::{Read, Seek};

    let empty = InitialTail {
        lines: Vec::new(),
        read: ReadState::default(),
    };

    let Ok(metadata) = std::fs::metadata(path) else {
        return empty;
    };
    let file_len = metadata.len();

    // 即使不回放（tail=0），增量读取也从文件当前末尾开始
    let mut read = ReadState {
        offset: file_len,
        partial: Vec::new(),
    };
    if file_len == 0 || tail == 0 {
        return InitialTail {
            lines: Vec::new(),
            read,
        };
    }

    let window_start = file_len.saturating_sub(MAX_TAIL_BYTES);
    let Ok(mut file) = std::fs::File::open(path) else {
        return empty;
    };
    if file.seek(SeekFrom::Start(window_start)).is_err() {
        return empty;
    }

    let mut data = Vec::with_capacity((file_len - window_start) as usize);
    if file.take(MAX_TAIL_BYTES).read_to_end(&mut data).is_err() {
        return empty;
    }

    // 窗口首行可能只有半截，丢弃到第一个换行之后
    let data = if window_start > 0 {
        match data.iter().position(|byte| *byte == b'\n') {
            Some(position) => &data[position + 1..],
            // 整个窗口是一行超长日志，无法给出完整行，等后续增量再说
            None => {
                read.partial.clear();
                return InitialTail {
                    lines: Vec::new(),
                    read,
                };
            }
        }
    } else {
        &data[..]
    };

    let window = tail_slice(data, tail);
    let (lines, partial) = split_complete_lines(window, usize::MAX);
    read.partial = partial;

    InitialTail { lines, read }
}

/// 构造 `event: log` 事件
fn log_event(lines: Vec<String>) -> Event {
    Event::default()
        .event("log")
        .data(serde_json::json!({ "lines": lines }).to_string())
}

/// 从当前偏移读取新增字节，返回可发送的完整行。
async fn read_new_lines(state: &mut StreamState, file_len: u64) -> Vec<String> {
    let Ok(mut file) = tokio::fs::File::open(&state.path).await else {
        return Vec::new();
    };
    if file.seek(SeekFrom::Start(state.read.offset)).await.is_err() {
        return Vec::new();
    }

    let to_read = (file_len - state.read.offset).min(MAX_READ_BYTES as u64) as usize;
    let mut chunk = vec![0u8; to_read];
    match file.read(&mut chunk).await {
        Ok(0) | Err(_) => Vec::new(),
        Ok(n) => {
            chunk.truncate(n);
            consume_chunk(&mut state.read, &chunk, MAX_LINES_PER_EVENT)
        }
    }
}

/// 轮询一次日志文件，返回本轮可发送的事件（无新行为 `None`）。
///
/// - 文件不存在：静默跳过，等下一轮重试；
/// - 文件变短：视为截断/轮转，偏移归零并清空半行缓冲，从头重读；
/// - 有新增字节：最多读 64 KiB，最多切 500 行；
/// - 上一轮因行数上限剩余的完整行，即使文件这轮没增长也继续发出。
async fn poll_once(state: &mut StreamState) -> Option<Event> {
    let mut lines = Vec::new();

    if let Ok(metadata) = tokio::fs::metadata(&state.path).await {
        let file_len = metadata.len();
        if is_truncated(file_len, state.read.offset) {
            state.read.offset = 0;
            state.read.partial.clear();
        }
        if file_len > state.read.offset {
            lines = read_new_lines(state, file_len).await;
        }
    }

    if lines.is_empty() {
        lines = consume_chunk(&mut state.read, &[], MAX_LINES_PER_EVENT);
    }

    (!lines.is_empty()).then(|| log_event(lines))
}

/// `GET /api/v1/logs/stream?tail=200`
///
/// SSE 端点，事件契约见模块文档。连接建立时同步读取首屏（最多 256 KiB），
/// 之后通过 `unfold` 每 250ms 轮询增量；`KeepAlive` 默认 15s 发送注释帧，
/// 防止反向代理因长时间无数据断开。
pub async fn stream_logs(
    Query(params): Query<HashMap<String, String>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tail = parse_tail(params.get("tail").map(String::as_str));
    let path = crate::logging::log_file_path();
    let initial = read_initial_tail(&path, tail);

    let mut pending = VecDeque::new();
    pending.push_back(
        Event::default().event("meta").data(
            serde_json::json!({
                "path": path.display().to_string(),
                "tail": tail,
            })
            .to_string(),
        ),
    );
    pending.push_back(log_event(initial.lines));

    let state = StreamState {
        path,
        read: initial.read,
        pending,
    };

    // 每个 step 先清空待发事件（meta / 首屏），再进入 250ms 轮询节奏。
    // 等待发生在 stream 内部：连接断开时 unfold 的 future 被 drop，不留后台任务。
    let stream = stream::unfold(state, |mut state| async move {
        if let Some(event) = state.pending.pop_front() {
            return Some((Some(Ok(event)), state));
        }

        tokio::time::sleep(POLL_INTERVAL).await;
        let event = poll_once(&mut state).await;
        Some((event.map(Ok), state))
    })
    // 没有新行时 unfold 产出 None，这里直接过滤掉，不向客户端发空事件
    .filter_map(|item| async move { item });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parse_tail_uses_default_and_clamps() {
        assert_eq!(parse_tail(None), DEFAULT_TAIL_LINES);
        assert_eq!(parse_tail(Some("50")), 50);
        assert_eq!(parse_tail(Some("0")), 0);
        assert_eq!(parse_tail(Some("1000")), 1000);
        assert_eq!(parse_tail(Some("999999")), MAX_TAIL_LINES);
        assert_eq!(parse_tail(Some("-1")), DEFAULT_TAIL_LINES);
        assert_eq!(parse_tail(Some("abc")), DEFAULT_TAIL_LINES);
        assert_eq!(parse_tail(Some("  42  ")), 42);
    }

    #[test]
    fn tail_window_keeps_last_bytes() {
        let data = b"0123456789";
        assert_eq!(tail_window(data, 4), b"6789");
        assert_eq!(tail_window(data, 0), b"");
        assert_eq!(tail_window(data, 100), data);
    }

    #[test]
    fn tail_slice_returns_last_n_lines() {
        let data = b"a\nb\nc\nd\n";
        assert_eq!(tail_slice(data, 2), b"c\nd\n");
        assert_eq!(tail_slice(data, 4), data);
        assert_eq!(tail_slice(data, 10), data);
        assert_eq!(tail_slice(data, 0), b"");
        assert_eq!(tail_slice(b"", 3), b"");
        // 末行没有换行时仍然算一行
        assert_eq!(tail_slice(b"a\nb", 1), b"b");
        // 只有一个末尾换行时不要多算一个空行
        assert_eq!(tail_slice(b"a\nb\n", 1), b"b\n");
    }

    #[test]
    fn split_complete_lines_keeps_partial_tail() {
        let (lines, rest) = split_complete_lines(b"a\nb\nc", usize::MAX);
        assert_eq!(lines, strings(&["a", "b"]));
        assert_eq!(rest, b"c");
    }

    #[test]
    fn split_complete_lines_handles_crlf_and_empty_lines() {
        let (lines, rest) = split_complete_lines(b"a\r\n\r\n", 10);
        assert_eq!(lines, strings(&["a", ""]));
        assert!(rest.is_empty());
    }

    #[test]
    fn split_complete_lines_keeps_rest_when_capped() {
        let (lines, rest) = split_complete_lines(b"a\nb\nc\n", 2);
        assert_eq!(lines, strings(&["a", "b"]));
        assert_eq!(rest, b"c\n");

        // 未消费的完整行下一轮还能继续切出来，不会丢
        let (lines, rest) = split_complete_lines(&rest, 2);
        assert_eq!(lines, strings(&["c"]));
        assert!(rest.is_empty());
    }

    #[test]
    fn consume_chunk_joins_partial_across_polls() {
        let mut state = ReadState::default();

        assert!(consume_chunk(&mut state, b"hel", 10).is_empty());
        assert_eq!(state.offset, 3);
        assert_eq!(state.partial, b"hel");

        let lines = consume_chunk(&mut state, b"lo\nworld\n", 10);
        assert_eq!(lines, strings(&["hello", "world"]));
        assert_eq!(state.offset, 12);
        assert!(state.partial.is_empty());
    }

    #[test]
    fn consume_chunk_advances_offset_by_read_bytes_even_without_newline() {
        let mut state = ReadState::default();
        assert!(consume_chunk(&mut state, b"abc", 10).is_empty());
        assert_eq!(state.offset, 3);
        assert_eq!(state.partial, b"abc");
    }

    #[test]
    fn consume_chunk_respects_event_line_cap_and_keeps_remainder() {
        let mut state = ReadState::default();
        let lines = consume_chunk(&mut state, b"a\nb\nc\n", 2);
        assert_eq!(lines, strings(&["a", "b"]));
        assert_eq!(state.offset, 6);
        assert_eq!(state.partial, b"c\n");

        // 下一轮没有新字节，也要能把剩余的完整行吐出来
        let lines = consume_chunk(&mut state, b"", 2);
        assert_eq!(lines, strings(&["c"]));
        assert!(state.partial.is_empty());
    }

    #[test]
    fn truncation_detected_only_when_file_shorter_than_offset() {
        assert!(is_truncated(5, 10));
        assert!(!is_truncated(10, 10));
        assert!(!is_truncated(11, 10));
    }

    #[test]
    fn read_initial_tail_returns_last_complete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        std::fs::write(&path, b"one\ntwo\nthree\n").unwrap();

        let initial = read_initial_tail(&path, 2);
        assert_eq!(initial.lines, strings(&["two", "three"]));
        assert_eq!(initial.read.offset, std::fs::metadata(&path).unwrap().len());
        assert!(initial.read.partial.is_empty());
    }

    #[test]
    fn read_initial_tail_keeps_unterminated_line_in_partial() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        std::fs::write(&path, b"one\ntwo\nthr").unwrap();

        let initial = read_initial_tail(&path, 3);
        // 残行 "thr" 先不展示，等后续写成完整行时只发一次
        assert_eq!(initial.lines, strings(&["one", "two"]));
        assert_eq!(initial.read.partial, b"thr");
        assert_eq!(initial.read.offset, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn read_initial_tail_with_zero_tail_starts_at_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        std::fs::write(&path, b"one\ntwo\n").unwrap();

        let initial = read_initial_tail(&path, 0);
        assert!(initial.lines.is_empty());
        assert_eq!(initial.read.offset, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn read_initial_tail_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.log");

        let initial = read_initial_tail(&path, 200);
        assert!(initial.lines.is_empty());
        assert_eq!(initial.read.offset, 0);
        assert!(initial.read.partial.is_empty());
    }

    #[test]
    fn read_initial_tail_caps_replay_to_last_256kb() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        // 400 行 × 约 1 KiB ≈ 400 KiB，超过首屏 256 KiB 的回读窗口
        let mut content = String::new();
        for index in 0..400 {
            content.push_str(&format!("line-{index:04}-{}\n", "x".repeat(1010)));
        }
        std::fs::write(&path, &content).unwrap();

        let initial = read_initial_tail(&path, MAX_TAIL_LINES);
        assert!(!initial.lines.is_empty());
        assert!(
            initial.lines.len() < 400,
            "超过 256 KiB 的历史行不应全部回放"
        );
        // 窗口首行可能被截断，必须丢弃到换行之后，返回的每一行都应完整
        assert!(initial.lines.iter().all(|line| line.starts_with("line-")));
        assert_eq!(initial.read.offset, std::fs::metadata(&path).unwrap().len());
    }

    #[tokio::test]
    async fn poll_once_reads_only_appended_bytes() {
        use std::fs::OpenOptions;
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        std::fs::write(&path, b"one\n").unwrap();

        let mut state = StreamState {
            path: path.clone(),
            read: ReadState {
                offset: std::fs::metadata(&path).unwrap().len(),
                partial: Vec::new(),
            },
            pending: VecDeque::new(),
        };

        // 没有新增 → 不产生事件
        assert!(poll_once(&mut state).await.is_none());

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "two").unwrap();
        drop(file);

        assert!(poll_once(&mut state).await.is_some());
        assert_eq!(state.read.offset, std::fs::metadata(&path).unwrap().len());
        assert!(state.read.partial.is_empty());
    }

    #[tokio::test]
    async fn poll_once_resets_after_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        std::fs::write(&path, b"new\n").unwrap();

        let mut state = StreamState {
            path: path.clone(),
            read: ReadState {
                // 旧文件读到了 100 字节，新文件只有 4 字节 → 判定为轮转
                offset: 100,
                partial: b"stale".to_vec(),
            },
            pending: VecDeque::new(),
        };

        assert!(poll_once(&mut state).await.is_some());
        assert_eq!(state.read.offset, std::fs::metadata(&path).unwrap().len());
        assert!(state.read.partial.is_empty(), "轮转后应清空半行缓冲");
    }
}
