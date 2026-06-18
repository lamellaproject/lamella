//! The Debug Adapter Protocol base wire format: the message envelope and its
//! `Content-Length` framing.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, Write};

/// A DAP protocol message: a request, a response, or an event (the `type` field
/// selects which).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Message {
    /// A request from the client (editor) to the adapter.
    Request(Request),
    /// The adapter's response to a request.
    Response(Response),
    /// An asynchronous event from the adapter (e.g. `stopped`, `output`).
    Event(Event),
}

/// A request from the client: a command and its arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The message sequence number.
    pub seq: i64,
    /// The command to run (e.g. `initialize`, `setBreakpoints`, `continue`).
    pub command: String,
    /// The command's arguments, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

/// The adapter's response to a [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// The message sequence number.
    pub seq: i64,
    /// The `seq` of the request this answers.
    pub request_seq: i64,
    /// Whether the request succeeded.
    pub success: bool,
    /// The command being answered.
    pub command: String,
    /// An error message when `success` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The command's result payload, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

/// An asynchronous event from the adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The message sequence number.
    pub seq: i64,
    /// The event name (e.g. `stopped`, `terminated`, `output`).
    pub event: String,
    /// The event payload, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

/// Writes a message with its `Content-Length` header and flushes.
///
/// # Errors
/// Returns an [`io::Error`] if serialization or the write fails.
pub fn write_message<W: Write>(writer: &mut W, message: &Message) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Reads one framed message, or `None` at a clean end of stream.
///
/// # Errors
/// Returns an [`io::Error`] if the framing is malformed (no `Content-Length`), the
/// stream ends mid-message, or the JSON does not parse.
pub fn read_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Message>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return if content_length.is_none() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended inside a message header",
                ))
            };
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.strip_prefix("Content-Length:") {
            content_length = value
                .trim()
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad Content-Length"))
                .map(Some)?;
        }
    }
    let length = content_length
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn a_request_serializes_with_its_type_tag() {
        let message = Message::Request(Request {
            seq: 1,
            command: "initialize".to_owned(),
            arguments: Some(json!({ "clientID": "vscode" })),
        });
        let text = serde_json::to_string(&message).unwrap();
        let back: Message = serde_json::from_str(&text).unwrap();
        assert_eq!(back, message);
        assert!(text.contains(r#""type":"request""#));
        assert!(text.contains(r#""command":"initialize""#));
    }

    #[test]
    fn a_response_omits_empty_optional_fields() {
        let message = Message::Response(Response {
            seq: 2,
            request_seq: 1,
            success: true,
            command: "configurationDone".to_owned(),
            message: None,
            body: None,
        });
        let text = serde_json::to_string(&message).unwrap();
        assert!(!text.contains("message"));
        assert!(!text.contains("body"));
        assert_eq!(serde_json::from_str::<Message>(&text).unwrap(), message);
    }

    #[test]
    fn framing_round_trips_through_a_byte_stream() {
        let message = Message::Event(Event {
            seq: 7,
            event: "stopped".to_owned(),
            body: Some(json!({ "reason": "breakpoint", "threadId": 1 })),
        });
        let mut buffer = Vec::new();
        write_message(&mut buffer, &message).unwrap();
        assert!(buffer.starts_with(b"Content-Length: "));

        let mut reader = Cursor::new(buffer);
        let read = read_message(&mut reader).unwrap();
        assert_eq!(read, Some(message));
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn reads_a_hand_written_frame() {
        let body = r#"{"type":"request","seq":3,"command":"continue"}"#;
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut reader = Cursor::new(frame.into_bytes());
        let message = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(
            message,
            Message::Request(Request {
                seq: 3,
                command: "continue".to_owned(),
                arguments: None,
            })
        );
    }

    #[test]
    fn a_frame_without_content_length_is_an_error() {
        let mut reader = Cursor::new(b"\r\n{}".to_vec());
        assert!(read_message(&mut reader).is_err());
    }
}
