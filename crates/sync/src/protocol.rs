//! Das NDJSON-Protokoll zwischen Sidecar und App.

use chrono::{DateTime, Utc};
use icloud_photos_core::model::PhotoRecord;
use serde::Deserialize;

/// Eine Zeile, die der Sidecar auf stdout schreibt.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarMessage {
    /// Der Sidecar wartet auf einen 2FA-Code auf stdin.
    AuthRequired {
        method: String,
    },
    AuthOk {
        #[serde(default)]
        trusted: bool,
    },
    Photo(PhotoRecord),
    Done {
        count: u64,
        #[serde(default)]
        cursor: Option<DateTime<Utc>>,
    },
    Error {
        #[serde(default = "unknown_kind")]
        kind: String,
        message: String,
    },
}

fn unknown_kind() -> String {
    "other".to_string()
}

/// Fortschrittsmeldung an die UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
    /// Die UI muss jetzt einen 2FA-Code abfragen.
    TwoFactorRequired,
    Authenticated {
        trusted: bool,
    },
    /// So viele Datensätze sind bereits im Katalog.
    Progress {
        stored: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_photo_line() {
        let line = r#"{"type":"photo","id":"a","filename":"IMG.HEIC","added_date":"2026-01-01T00:00:00+00:00","asset_date":null,"checksum":"c","width":4032,"height":3024}"#;
        let msg: SidecarMessage = serde_json::from_str(line).unwrap();
        let SidecarMessage::Photo(record) = msg else {
            panic!("erwartet: Photo");
        };
        assert_eq!(record.id, "a");
        assert_eq!(record.checksum.as_deref(), Some("c"));
    }

    #[test]
    fn parses_control_lines() {
        let cases = [
            (
                r#"{"type":"auth_required","method":"2fa"}"#,
                SidecarMessage::AuthRequired {
                    method: "2fa".into(),
                },
            ),
            (
                r#"{"type":"auth_ok","trusted":true}"#,
                SidecarMessage::AuthOk { trusted: true },
            ),
            (
                r#"{"type":"error","kind":"auth","message":"nope"}"#,
                SidecarMessage::Error {
                    kind: "auth".into(),
                    message: "nope".into(),
                },
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(
                serde_json::from_str::<SidecarMessage>(line).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn parses_done_with_and_without_cursor() {
        let msg: SidecarMessage = serde_json::from_str(
            r#"{"type":"done","count":3,"cursor":"2026-01-01T00:02:00+00:00"}"#,
        )
        .unwrap();
        assert!(matches!(
            msg,
            SidecarMessage::Done {
                count: 3,
                cursor: Some(_)
            }
        ));

        let msg: SidecarMessage =
            serde_json::from_str(r#"{"type":"done","count":0,"cursor":null}"#).unwrap();
        assert!(matches!(
            msg,
            SidecarMessage::Done {
                count: 0,
                cursor: None
            }
        ));
    }

    #[test]
    fn rejects_unknown_message_type() {
        assert!(serde_json::from_str::<SidecarMessage>(r#"{"type":"bogus"}"#).is_err());
    }
}
