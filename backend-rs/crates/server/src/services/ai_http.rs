//! Bounded HTTP helpers shared by the AI adapters (port of `services/aiHttp.ts`): capped body
//! reads, SSE `data:` extraction with a per-event size limit, and text sanitising.

use std::sync::LazyLock;
use std::time::Duration;

use futures_util::StreamExt;
use inboxora_net::reqwest;
use serde_json::Value;

/// A plain client for fixed, public provider endpoints (Codex OAuth and Responses).
pub static PUBLIC: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build().expect("TLS backend available")
});

/// `sanitizeText`: control characters become spaces, whitespace collapses, capped length.
pub fn sanitize_text(value: &str, max: usize) -> String {
    let replaced: String = value
        .chars()
        .map(|c| if (c as u32) <= 0x1f || (0x7f..=0x9f).contains(&(c as u32)) { ' ' } else { c })
        .collect();
    let collapsed = inboxora_mail::text::collapse_ws(&replaced);
    inboxora_mail::text::js_substring(collapsed.trim(), max)
}

/// `parseJson`: an object or array, else `None`.
pub fn parse_json(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text).ok().filter(|v| v.is_object() || v.is_array())
}

/// `readLimited`: at most `limit` bytes of the body, decoded as UTF-8.
pub async fn read_limited(response: reqwest::Response, limit: usize) -> Result<String, reqwest::Error> {
    let mut stream = response.bytes_stream();
    let mut out: Vec<u8> = Vec::new();
    while out.len() < limit {
        let Some(chunk) = stream.next().await else { break };
        let chunk = chunk?;
        let take = chunk.len().min(limit - out.len());
        out.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Why an SSE read stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseError {
    EmptyBody,
    EventTooLarge,
    Transport,
}

fn block_data(block: &[u8]) -> String {
    String::from_utf8_lossy(block)
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|l| l.strip_prefix(' ').unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn boundary(buf: &[u8]) -> Option<(usize, usize)> {
    (0..buf.len()).find_map(|i| {
        let rest = &buf[i..];
        if rest.starts_with(b"\r\n\r\n") {
            Some((i, 4))
        } else if rest.starts_with(b"\n\r\n") || rest.starts_with(b"\r\n\n") {
            Some((i, 3))
        } else if rest.starts_with(b"\n\n") {
            Some((i, 2))
        } else {
            None
        }
    })
}

/// `readSseData`: each event's joined `data:` lines, handed to `on_data` until it returns
/// `false` (stop) or the body ends.
pub async fn read_sse_data(
    response: reqwest::Response,
    max_event_bytes: usize,
    mut on_data: impl FnMut(String) -> std::ops::ControlFlow<()>,
) -> Result<(), SseError> {
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    let mut any = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| SseError::Transport)?;
        any = true;
        buffer.extend_from_slice(&chunk);
        while let Some((at, len)) = boundary(&buffer) {
            if at > max_event_bytes {
                return Err(SseError::EventTooLarge);
            }
            let block: Vec<u8> = buffer.drain(..at + len).take(at).collect();
            let data = block_data(&block);
            if !data.is_empty() && on_data(data).is_break() {
                return Ok(());
            }
        }
        if buffer.len() > max_event_bytes {
            return Err(SseError::EventTooLarge);
        }
    }
    if !any && buffer.is_empty() {
        return Err(SseError::EmptyBody);
    }
    if !String::from_utf8_lossy(&buffer).trim().is_empty() {
        let data = block_data(&buffer);
        if !data.is_empty() {
            let _ = on_data(data);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_and_extracts_data() {
        assert_eq!(sanitize_text("a\u{0}b\n\n c\u{85}", 1000), "a b c");
        assert_eq!(sanitize_text("abcdef", 3), "abc");
        assert_eq!(block_data(b"event: x\r\ndata: {\"a\":1}\r\ndata:two"), "{\"a\":1}\ntwo");
        assert_eq!(boundary(b"data: 1\r\n\r\ndata: 2"), Some((7, 4)));
        assert_eq!(parse_json("3"), None);
    }
}
