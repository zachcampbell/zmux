// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Human-facing inspection and replay helpers for diagnostic traces.
//!
//! `inspect_trace` is deliberately safe to use on arbitrary captured bytes:
//! binary payloads are escaped and bounded before they reach the terminal.
//! `replay_trace` is the explicit rendering-oriented operation; it writes the
//! logical rows from the last recorded server frame, including their ANSI
//! styling, after printing the frame metadata.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use crate::mouse::MouseTrackingMode;
use crate::protocol::{ServerDecoder, ServerMessage};
use crate::pty::PtySize;
use crate::trace::{TraceKind, TracePayload, TraceReader};

const BYTE_PREVIEW_LIMIT: usize = 48;
const JSON_PREVIEW_LIMIT: usize = 160;

#[derive(Debug, Default)]
struct KindTotal {
    records: u64,
    payload_bytes: u64,
}

#[derive(Debug)]
struct FrameSnapshot {
    seq: u64,
    elapsed_ns: u64,
    size: PtySize,
    mouse_tracking_mode: MouseTrackingMode,
    lines: Vec<String>,
    cursor: Option<(u16, u16)>,
}

/// Print an ordered, bounded summary of every record in a trace.
///
/// `path` may be a trace bundle directory or its event stream file; path
/// resolution and validation are delegated to [`TraceReader`]. No raw binary
/// byte is written to `out` by this function.
pub fn inspect_trace(path: &Path, out: &mut dyn Write) -> io::Result<()> {
    let reader = TraceReader::open(path)?;
    let mut totals: BTreeMap<String, KindTotal> = BTreeMap::new();
    let mut record_count = 0_u64;
    let mut payload_bytes = 0_u64;

    for record in reader {
        let record = match record {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                writeln!(out, "warning: ignored crash-truncated trace tail: {error}")?;
                break;
            }
            Err(error) => return Err(error),
        };
        let kind = format!("{:?}", record.kind);
        let (payload_len, payload_summary) = summarize_payload(&record.payload);
        let context = format!("{:?}", record.context);

        writeln!(
            out,
            "#{:06} +{} {:<18} {} {}",
            record.seq,
            format_elapsed(record.elapsed_ns),
            kind,
            context,
            payload_summary,
        )?;

        record_count = record_count.saturating_add(1);
        payload_bytes = payload_bytes.saturating_add(payload_len as u64);
        let total = totals.entry(kind).or_default();
        total.records = total.records.saturating_add(1);
        total.payload_bytes = total.payload_bytes.saturating_add(payload_len as u64);
    }

    writeln!(out)?;
    writeln!(
        out,
        "totals: {record_count} records, {payload_bytes} payload bytes"
    )?;
    for (kind, total) in totals {
        writeln!(
            out,
            "  {kind:<18} {:>8} records {:>12} bytes",
            total.records, total.payload_bytes
        )?;
    }

    Ok(())
}

/// Decode and print the last logical server frame recorded in a trace.
///
/// Frame rows intentionally retain their recorded ANSI styling. Use
/// [`inspect_trace`] when a safely escaped byte-level view is desired.
pub fn replay_trace(path: &Path, out: &mut dyn Write) -> io::Result<()> {
    let reader = TraceReader::open(path)?;
    let mut last_record = None;
    let mut truncated_tail = None;

    for record in reader {
        let record = match record {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                truncated_tail = Some(error.to_string());
                break;
            }
            Err(error) => return Err(error),
        };
        if !matches!(&record.kind, TraceKind::ServerFrame) {
            continue;
        }

        // Do not decode while scanning: a malformed historical frame should
        // not prevent replaying a later valid checkpoint. Only the final
        // complete ServerFrame record is authoritative for this command.
        last_record = Some((record.seq, record.elapsed_ns, record.payload));
    }

    let (seq, elapsed_ns, payload) = last_record.ok_or_else(|| {
        invalid_data(
            "trace contains no ServerFrame records; capture while a client is attached and a frame is being rendered",
        )
    })?;
    let bytes = match &payload {
        TracePayload::Bytes(bytes) => bytes.as_slice(),
        TracePayload::Json(_) => {
            return Err(invalid_data(format!(
                "ServerFrame record #{seq} has a JSON payload; expected protocol-encoded bytes"
            )));
        }
    };

    let frames = decode_server_frames(seq, bytes)?;
    let frame = frames.into_iter().last().ok_or_else(|| {
        invalid_data(format!(
            "ServerFrame record #{seq} decoded successfully but contained no Frame message"
        ))
    })?;
    if let Some(error) = truncated_tail {
        writeln!(out, "warning: ignored crash-truncated trace tail: {error}")?;
    }
    write_frame(
        out,
        &FrameSnapshot {
            seq,
            elapsed_ns,
            size: frame.size,
            mouse_tracking_mode: frame.mouse_tracking_mode,
            lines: frame.lines,
            cursor: frame.cursor,
        },
    )
}

#[derive(Debug)]
struct DecodedFrame {
    size: PtySize,
    mouse_tracking_mode: MouseTrackingMode,
    lines: Vec<String>,
    cursor: Option<(u16, u16)>,
}

fn decode_server_frames(seq: u64, bytes: &[u8]) -> io::Result<Vec<DecodedFrame>> {
    validate_complete_protocol_stream(seq, bytes)?;

    let mut decoder = ServerDecoder::default();
    let messages = decoder.push_bytes(bytes).map_err(|error| {
        invalid_data(format!(
            "cannot decode ServerFrame record #{seq} as a server protocol message: {error}"
        ))
    })?;
    let mut frames = Vec::new();
    for message in messages {
        match message {
            ServerMessage::Frame {
                size,
                mouse_tracking_mode,
                lines,
                cursor,
            } => frames.push(DecodedFrame {
                size,
                mouse_tracking_mode,
                lines,
                cursor,
            }),
            other => {
                return Err(invalid_data(format!(
                    "ServerFrame record #{seq} contains a non-frame server message ({})",
                    server_message_name(&other)
                )));
            }
        }
    }
    Ok(frames)
}

fn server_message_name(message: &ServerMessage) -> &'static str {
    match message {
        ServerMessage::Frame { .. } => "Frame",
        ServerMessage::Exited { .. } => "Exited",
        ServerMessage::Error(_) => "Error",
        ServerMessage::Busy => "Busy",
        ServerMessage::Clipboard(_) => "Clipboard",
        ServerMessage::PaneList(_) => "PaneList",
        ServerMessage::TraceStatus { .. } => "TraceStatus",
    }
}

// `ServerDecoder` correctly buffers partial network input, but a trace record
// is specified to contain complete protocol messages. Validate framing first
// so a truncated tail cannot be silently left in a short-lived decoder.
fn validate_complete_protocol_stream(seq: u64, bytes: &[u8]) -> io::Result<()> {
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        if remaining.len() < 4 {
            return Err(invalid_data(format!(
                "ServerFrame record #{seq} ends with an incomplete protocol length prefix"
            )));
        }
        let body_len =
            u32::from_le_bytes([remaining[0], remaining[1], remaining[2], remaining[3]]) as usize;
        let frame_len = 4_usize.checked_add(body_len).ok_or_else(|| {
            invalid_data(format!(
                "ServerFrame record #{seq} has an overflowing protocol frame length"
            ))
        })?;
        if remaining.len() < frame_len {
            return Err(invalid_data(format!(
                "ServerFrame record #{seq} is truncated: frame needs {frame_len} bytes, only {} remain",
                remaining.len()
            )));
        }
        offset += frame_len;
    }
    Ok(())
}

fn write_frame(out: &mut dyn Write, frame: &FrameSnapshot) -> io::Result<()> {
    let cursor = match frame.cursor {
        Some((row, col)) => format!("{row},{col} (row,col; 1-based)"),
        None => "hidden".to_string(),
    };
    writeln!(out, "last ServerFrame")?;
    writeln!(out, "  seq: {}", frame.seq)?;
    writeln!(out, "  elapsed: {}", format_elapsed(frame.elapsed_ns))?;
    writeln!(
        out,
        "  size: {}x{} (cols x rows)",
        frame.size.cols, frame.size.rows
    )?;
    writeln!(out, "  lines: {}", frame.lines.len())?;
    writeln!(out, "  cursor: {cursor}")?;
    writeln!(out, "  mouse: {:?}", frame.mouse_tracking_mode)?;
    writeln!(out, "--- frame ---")?;
    for line in &frame.lines {
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")?;
    }
    writeln!(out, "--- end frame ---")
}

fn summarize_payload(payload: &TracePayload) -> (usize, String) {
    match payload {
        TracePayload::Bytes(bytes) => (
            bytes.len(),
            format!(
                "bytes={} {}",
                bytes.len(),
                summarize_bytes(bytes, BYTE_PREVIEW_LIMIT)
            ),
        ),
        TracePayload::Json(value) => {
            let encoded = value.to_string();
            (
                encoded.len(),
                format!(
                    "json={} {}",
                    encoded.len(),
                    truncate_text(&encoded, JSON_PREVIEW_LIMIT)
                ),
            )
        }
    }
}

fn summarize_bytes(bytes: &[u8], limit: usize) -> String {
    let shown = bytes.len().min(limit);
    let mut summary = String::with_capacity(shown.saturating_mul(2).saturating_add(3));
    summary.push('"');
    for &byte in &bytes[..shown] {
        match byte {
            b'\n' => summary.push_str("\\n"),
            b'\r' => summary.push_str("\\r"),
            b'\t' => summary.push_str("\\t"),
            b'\\' => summary.push_str("\\\\"),
            b'"' => summary.push_str("\\\""),
            0x20..=0x7e => summary.push(char::from(byte)),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(summary, "\\x{byte:02x}");
            }
        }
    }
    summary.push('"');
    if shown < bytes.len() {
        summary.push_str(&format!("...(+{} bytes)", bytes.len() - shown));
    }
    summary
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(limit).collect();
    truncated.push_str("...");
    truncated
}

fn format_elapsed(elapsed_ns: u64) -> String {
    format!(
        "{}.{:09}s",
        elapsed_ns / 1_000_000_000,
        elapsed_ns % 1_000_000_000
    )
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::mouse::MouseTrackingMode;
    use crate::protocol::{ServerMessage, encode_server_message};
    use crate::pty::PtySize;
    use crate::trace::{TraceContext, TraceHub, TraceKind, TraceStartOptions};

    use super::{
        FrameSnapshot, decode_server_frames, format_elapsed, inspect_trace, replay_trace,
        summarize_bytes, validate_complete_protocol_stream, write_frame,
    };

    fn temp_bundle(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "zmux-trace-tools-{}-{name}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn inspect_prints_escaped_timeline_and_totals() {
        let bundle = temp_bundle("inspect");
        let hub = TraceHub::new("inspect");
        hub.start(TraceStartOptions {
            output: Some(bundle.clone()),
            max_bytes: 1024 * 1024,
        })
        .unwrap();
        hub.record_bytes(
            TraceKind::ClientInput,
            TraceContext {
                client_id: Some(7),
                ..TraceContext::default()
            },
            b"touch\x1b[<0;3;4M",
        );
        hub.stop();

        let mut out = Vec::new();
        inspect_trace(&bundle, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("ClientInput"));
        assert!(text.contains("client_id: Some(7)"));
        assert!(text.contains("touch\\x1b[<0;3;4M"));
        assert!(text.contains("totals:"));
        assert!(!text.as_bytes().contains(&0x1b));

        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn replay_uses_last_frame_and_ignores_malformed_historical_frame() {
        let bundle = temp_bundle("replay-last");
        let hub = TraceHub::new("replay-last");
        hub.start(TraceStartOptions {
            output: Some(bundle.clone()),
            max_bytes: 1024 * 1024,
        })
        .unwrap();
        hub.record_json(
            TraceKind::ServerFrame,
            TraceContext::default(),
            &serde_json::json!({ "malformed": "historical" }),
        );
        for line in ["first frame", "last frame"] {
            let encoded = encode_server_message(&ServerMessage::Frame {
                size: PtySize::new(3, 20),
                mouse_tracking_mode: MouseTrackingMode::Click,
                lines: vec![line.into()],
                cursor: Some((1, 2)),
            })
            .unwrap();
            hub.record_bytes(TraceKind::ServerFrame, TraceContext::default(), &encoded);
        }
        hub.stop();

        let mut out = Vec::new();
        replay_trace(&bundle, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("last frame"));
        assert!(!text.contains("first frame"));
        assert!(text.contains("mouse: Click"));

        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn replay_without_a_frame_has_actionable_error() {
        let bundle = temp_bundle("replay-empty");
        let hub = TraceHub::new("replay-empty");
        hub.start(TraceStartOptions {
            output: Some(bundle.clone()),
            max_bytes: 1024 * 1024,
        })
        .unwrap();
        hub.stop();

        let error = replay_trace(&bundle, &mut Vec::new()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("capture while a client is attached")
        );

        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn binary_summary_escapes_terminal_controls_and_is_bounded() {
        let bytes = b"hello\x1b[31m\n\\\"world\xfftail";
        let summary = summarize_bytes(bytes, 10);

        assert_eq!(summary, "\"hello\\x1b[31m\"...(+13 bytes)");
        assert!(!summary.as_bytes().contains(&0x1b));
    }

    #[test]
    fn elapsed_time_is_exact_without_float_rounding() {
        assert_eq!(format_elapsed(0), "0.000000000s");
        assert_eq!(format_elapsed(12_345_678_901), "12.345678901s");
    }

    #[test]
    fn server_frame_payload_decodes_with_protocol_decoder() {
        let encoded = encode_server_message(&ServerMessage::Frame {
            size: PtySize::new(24, 80),
            mouse_tracking_mode: MouseTrackingMode::Drag,
            lines: vec!["first".into(), "second".into()],
            cursor: Some((2, 7)),
        })
        .expect("encode frame");

        let frames = decode_server_frames(41, &encoded).expect("decode trace frame");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].size, PtySize::new(24, 80));
        assert_eq!(frames[0].mouse_tracking_mode, MouseTrackingMode::Drag);
        assert_eq!(frames[0].lines, ["first", "second"]);
        assert_eq!(frames[0].cursor, Some((2, 7)));
    }

    #[test]
    fn truncated_frame_payload_reports_record_sequence() {
        let mut encoded = encode_server_message(&ServerMessage::Frame {
            size: PtySize::new(1, 4),
            mouse_tracking_mode: MouseTrackingMode::Off,
            lines: vec!["test".into()],
            cursor: None,
        })
        .expect("encode frame");
        encoded.pop();

        let error = validate_complete_protocol_stream(99, &encoded).expect_err("truncated");
        let message = error.to_string();
        assert!(message.contains("#99"), "{message}");
        assert!(message.contains("truncated"), "{message}");
    }

    #[test]
    fn frame_output_includes_rendering_metadata_and_rows() {
        let frame = FrameSnapshot {
            seq: 8,
            elapsed_ns: 2_500_000_000,
            size: PtySize::new(2, 10),
            mouse_tracking_mode: MouseTrackingMode::Click,
            lines: vec!["alpha".into(), "beta".into()],
            cursor: Some((2, 4)),
        };
        let mut output = Vec::new();
        write_frame(&mut output, &frame).expect("write frame");
        let output = String::from_utf8(output).expect("utf8 output");

        assert!(output.contains("size: 10x2 (cols x rows)"), "{output}");
        assert!(
            output.contains("cursor: 2,4 (row,col; 1-based)"),
            "{output}"
        );
        assert!(output.contains("mouse: Click"), "{output}");
        assert!(output.contains("--- frame ---\nalpha\nbeta\n--- end frame ---"));
    }
}
