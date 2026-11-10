//! `/v1/chat/completions` の逐次表示 (Server-Sent Events) を読む。
//!
//! 出来事は空行で区切られ、`data:` の行に JSON (最後は `[DONE]`) が入る。

use serde_json::Value;

use crate::LlmError;
use crate::types::{ChatEvent, Usage};

/// バイト列を受け取って出来事に分ける。
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    done: bool,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// `[DONE]` を受け取った。
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// 溜まったバイト列から、次の出来事を取り出す。足りなければ `None`。
    pub fn next_event(&mut self) -> Option<Result<ChatEvent, LlmError>> {
        loop {
            if self.done {
                return None;
            }
            let (end, sep) = find_block_end(&self.buf)?;
            let block: Vec<u8> = self.buf.drain(..end + sep).take(end).collect();
            let text = match String::from_utf8(block) {
                Ok(t) => t,
                Err(_) => return Some(Err(LlmError::Protocol("event is not UTF-8".into()))),
            };
            let data: Vec<&str> = text
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if data.is_empty() {
                // コメント (`:`) や、ほかの欄だけの出来事。
                continue;
            }
            let payload = data.join("\n");
            if payload.trim() == "[DONE]" {
                self.done = true;
                return Some(Ok(ChatEvent::Done {
                    finish_reason: self.finish_reason.take(),
                    usage: self.usage.take(),
                }));
            }
            match self.parse(&payload) {
                Some(r) => return Some(r),
                None => continue,
            }
        }
    }

    fn parse(&mut self, payload: &str) -> Option<Result<ChatEvent, LlmError>> {
        let v: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => return Some(Err(LlmError::Protocol(format!("bad event JSON: {e}")))),
        };
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| err.to_string(), str::to_owned);
            return Some(Err(LlmError::Server(msg)));
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = serde_json::from_value(u.clone()).ok();
        }
        let choice = v.get("choices").and_then(|c| c.get(0))?;
        if let Some(r) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(r.to_owned());
        }
        let delta = choice.get("delta")?;
        if let Some(c) = delta.get("content").and_then(Value::as_str)
            && !c.is_empty()
        {
            return Some(Ok(ChatEvent::Delta(c.to_owned())));
        }
        if let Some(c) = delta.get("reasoning_content").and_then(Value::as_str)
            && !c.is_empty()
        {
            return Some(Ok(ChatEvent::Reasoning(c.to_owned())));
        }
        None
    }
}

/// 出来事の終わり (空行) の位置と、区切りの長さ。
fn find_block_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == b'\n' {
            if buf.get(i + 1) == Some(&b'\n') {
                return Some((i, 2));
            }
            if buf.get(i + 1..i + 3) == Some(b"\r\n") {
                return Some((i, 3));
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(p: &mut SseParser) -> Vec<Result<ChatEvent, LlmError>> {
        std::iter::from_fn(|| p.next_event()).collect()
    }

    fn chunk(content: &str) -> String {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
            serde_json::to_string(content).unwrap()
        )
    }

    #[test]
    fn reads_deltas_across_split_chunks_and_done() {
        let stream = format!(
            "{}{}: keep-alive\n\n{}data: [DONE]\n\n",
            chunk("こん"),
            chunk("にちは"),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n"
        );
        let bytes = stream.as_bytes();
        let mut p = SseParser::default();
        let mut events = Vec::new();
        // 1 バイトずつ渡しても (UTF-8 の途中で切れても) 同じに読める。
        for b in bytes {
            p.push(std::slice::from_ref(b));
            events.extend(all(&mut p));
        }
        assert_eq!(
            events.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
            vec![
                ChatEvent::Delta("こん".into()),
                ChatEvent::Delta("にちは".into()),
                ChatEvent::Done {
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        prompt_tokens: 5,
                        completion_tokens: 2
                    })
                },
            ]
        );
        assert!(p.is_done());
    }

    #[test]
    fn crlf_separators_and_reasoning_are_read() {
        let mut p = SseParser::default();
        p.push(b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\r\n");
        assert_eq!(all(&mut p), vec![Ok(ChatEvent::Reasoning("think".into()))]);
    }

    #[test]
    fn server_errors_in_the_stream_are_reported() {
        let mut p = SseParser::default();
        p.push(b"data: {\"error\":{\"message\":\"context full\"}}\n\n");
        assert_eq!(
            all(&mut p),
            vec![Err(LlmError::Server("context full".into()))]
        );
        let mut p = SseParser::default();
        p.push(b"data: {not json}\n\n");
        assert!(matches!(all(&mut p)[..], [Err(LlmError::Protocol(_))]));
    }
}
