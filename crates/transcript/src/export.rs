use std::io::{BufRead, BufReader, Read, Write};

use agentctl_core::{CanonicalEvent, UnifiedSession, UnifiedSessionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Result, TranscriptError};

pub const EXPORT_FORMAT_VERSION: u32 = 1;
const DEFAULT_MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExportBlob {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
    pub included: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExportManifest {
    pub format_version: u32,
    pub session: UnifiedSession,
    pub event_count: usize,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub events_checksum: String,
    pub blobs: Vec<ExportBlob>,
    pub exported_at: DateTime<Utc>,
    pub redacted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExportBundle {
    pub manifest: ExportManifest,
    pub events: Vec<CanonicalEvent>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "record", rename_all = "snake_case")]
enum ExportRecord {
    Manifest { manifest: ExportManifest },
    Event { event: CanonicalEvent },
}

pub fn build_export(
    session: UnifiedSession,
    events: Vec<CanonicalEvent>,
    blobs: Vec<ExportBlob>,
    redacted: bool,
    exported_at: DateTime<Utc>,
) -> Result<ExportBundle> {
    validate_events(session.id, &events)?;
    let events_checksum = events_checksum(&events)?;
    Ok(ExportBundle {
        manifest: ExportManifest {
            format_version: EXPORT_FORMAT_VERSION,
            event_count: events.len(),
            first_seq: events.first().map(|event| event.seq),
            last_seq: events.last().map(|event| event.seq),
            events_checksum,
            blobs,
            exported_at,
            redacted,
            session,
        },
        events,
    })
}

pub fn write_jsonl(writer: &mut impl Write, bundle: &ExportBundle) -> Result<()> {
    validate_bundle(bundle)?;
    serde_json::to_writer(
        &mut *writer,
        &ExportRecord::Manifest {
            manifest: bundle.manifest.clone(),
        },
    )?;
    writer.write_all(b"\n")?;
    for event in &bundle.events {
        serde_json::to_writer(
            &mut *writer,
            &ExportRecord::Event {
                event: event.clone(),
            },
        )?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

pub fn read_jsonl(reader: impl Read) -> Result<ExportBundle> {
    read_jsonl_with_limit(reader, DEFAULT_MAX_LINE_BYTES)
}

pub fn read_jsonl_with_limit(reader: impl Read, max_line_bytes: usize) -> Result<ExportBundle> {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let mut manifest = None;
    let mut events = Vec::new();
    loop {
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break;
        }
        if line.len() > max_line_bytes {
            return Err(TranscriptError::LineTooLarge(max_line_bytes));
        }
        while line
            .last()
            .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
        {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<ExportRecord>(&line)? {
            ExportRecord::Manifest { manifest: value } => {
                if manifest.replace(value).is_some() {
                    return Err(TranscriptError::InvalidExport(
                        "more than one manifest record".to_owned(),
                    ));
                }
                if !events.is_empty() {
                    return Err(TranscriptError::InvalidExport(
                        "manifest must be the first record".to_owned(),
                    ));
                }
            }
            ExportRecord::Event { event } => {
                if manifest.is_none() {
                    return Err(TranscriptError::InvalidExport(
                        "event appears before manifest".to_owned(),
                    ));
                }
                events.push(event);
            }
        }
    }
    let bundle = ExportBundle {
        manifest: manifest
            .ok_or_else(|| TranscriptError::InvalidExport("manifest is missing".to_owned()))?,
        events,
    };
    validate_bundle(&bundle)?;
    Ok(bundle)
}

fn validate_bundle(bundle: &ExportBundle) -> Result<()> {
    if bundle.manifest.format_version != EXPORT_FORMAT_VERSION {
        return Err(TranscriptError::InvalidExport(format!(
            "unsupported format version {}",
            bundle.manifest.format_version
        )));
    }
    validate_events(bundle.manifest.session.id, &bundle.events)?;
    if bundle.manifest.event_count != bundle.events.len()
        || bundle.manifest.first_seq != bundle.events.first().map(|event| event.seq)
        || bundle.manifest.last_seq != bundle.events.last().map(|event| event.seq)
    {
        return Err(TranscriptError::InvalidExport(
            "manifest event bounds do not match records".to_owned(),
        ));
    }
    let actual = events_checksum(&bundle.events)?;
    if actual != bundle.manifest.events_checksum {
        return Err(TranscriptError::ChecksumMismatch {
            expected: bundle.manifest.events_checksum.clone(),
            actual,
        });
    }
    Ok(())
}

fn validate_events(session_id: UnifiedSessionId, events: &[CanonicalEvent]) -> Result<()> {
    let mut previous = 0;
    for event in events {
        if event.session_id != session_id {
            return Err(TranscriptError::InvalidExport(format!(
                "event {} belongs to another session",
                event.event_id
            )));
        }
        if event.seq <= previous {
            return Err(TranscriptError::NonMonotonic(event.seq));
        }
        previous = event.seq;
    }
    Ok(())
}

fn events_checksum(events: &[CanonicalEvent]) -> Result<String> {
    let mut hasher = Sha256::new();
    for event in events {
        let encoded = serde_json::to_vec(event)?;
        hasher.update((encoded.len() as u64).to_be_bytes());
        hasher.update(encoded);
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use agentctl_core::{AuthMode, EventId, EventVisibility, SessionStatus, UnifiedSessionId};

    use super::*;

    fn session() -> UnifiedSession {
        let now = Utc::now();
        UnifiedSession {
            id: UnifiedSessionId::new(),
            name: "export".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/export"),
            workspace_fingerprint: "sha256:test".to_owned(),
            active_provider: None,
            routing_policy: "manual".to_owned(),
            auth_mode: AuthMode::NativeLocal,
            status: SessionStatus::Active,
            parent_session_id: None,
            created_at: now,
            updated_at: now,
            schema_version: 1,
        }
    }

    #[test]
    fn jsonl_round_trip_validates_checksum() {
        let session = session();
        let event = CanonicalEvent {
            schema_version: 1,
            session_id: session.id,
            seq: 1,
            event_id: EventId::new(),
            turn_id: None,
            origin_provider: None,
            kind: "user_prompt".to_owned(),
            visibility: EventVisibility::User,
            payload: serde_json::json!({"text": "hello"}),
            content_hash: "sha256:test".to_owned(),
            raw_event_id: None,
            created_at: Utc::now(),
        };
        let bundle = build_export(session, vec![event], Vec::new(), false, Utc::now()).unwrap();
        let mut bytes = Vec::new();
        write_jsonl(&mut bytes, &bundle).unwrap();
        let imported = read_jsonl(bytes.as_slice()).unwrap();
        assert_eq!(imported.events.len(), 1);
        assert_eq!(
            imported.manifest.events_checksum,
            bundle.manifest.events_checksum
        );
    }

    #[test]
    fn rejects_oversized_lines() {
        let error = read_jsonl_with_limit("{\"large\":true}\n".as_bytes(), 3).unwrap_err();
        assert!(matches!(error, TranscriptError::LineTooLarge(3)));
    }

    #[test]
    fn rejects_tampered_event_payload() {
        let session = session();
        let event = CanonicalEvent {
            schema_version: 1,
            session_id: session.id,
            seq: 1,
            event_id: EventId::new(),
            turn_id: None,
            origin_provider: None,
            kind: "user_prompt".to_owned(),
            visibility: EventVisibility::User,
            payload: serde_json::json!({"text": "hello"}),
            content_hash: "sha256:test".to_owned(),
            raw_event_id: None,
            created_at: Utc::now(),
        };
        let bundle = build_export(session, vec![event], Vec::new(), false, Utc::now()).unwrap();
        let mut bytes = Vec::new();
        write_jsonl(&mut bytes, &bundle).unwrap();
        let text = String::from_utf8(bytes)
            .unwrap()
            .replace("hello", "tampered");
        assert!(matches!(
            read_jsonl(text.as_bytes()),
            Err(TranscriptError::ChecksumMismatch { .. })
        ));
    }
}
