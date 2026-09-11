//! Das NDJSON-Protokoll zwischen Sidecar und App.

use chrono::{DateTime, Utc};
use icloud_photos_core::model::PhotoRecord;
use serde::Deserialize;

/// Eine Zeile, die der Sidecar auf stdout schreibt.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarMessage {
    /// Der Sidecar hat kein Passwort im Keyring und wartet auf eines auf stdin.
    PasswordRequired {
        #[serde(default)]
        username: String,
    },
    /// Der Sidecar wartet auf einen 2FA-Code auf stdin.
    AuthRequired {
        method: String,
    },
    AuthOk {
        #[serde(default)]
        trusted: bool,
    },
    /// Zone-Token vor dem Durchlauf. Stimmt er beim naechsten Mal noch, hat
    /// sich in der Bibliothek nichts getan.
    Cursor {
        #[serde(default)]
        token: Option<String>,
    },
    /// Token unveraendert — nichts zu tun.
    Unchanged,
    Photo(PhotoRecord),
    /// In iCloud geloescht.
    Deleted {
        id: String,
    },
    /// Fortschritt des Erstaufbaus, damit ein Abbruch nicht alles kostet.
    Offset {
        value: u64,
    },
    /// Wie viele Assets die Bibliothek insgesamt hat — Bestandsabgleich nach
    /// einem Delta-Lauf.
    LibraryCount {
        value: u64,
    },
    BaselineComplete {
        count: u64,
    },
    /// Der Delta-Abgleich war nicht moeglich; der Sidecar faellt selbst auf
    /// einen vollstaendigen Durchlauf zurueck.
    TokenExpired {
        #[serde(default)]
        message: String,
    },
    /// Ein geaenderter Record liess sich keinem Asset zuordnen — in der Zone
    /// liegen auch Alben und Hilfsrecords.
    ChangeUnresolved {
        id: String,
        #[serde(default)]
        record_type: Option<String>,
        #[serde(default)]
        message: String,
    },
    /// Eine Vorschau wurde geladen und liegt unter `path`.
    Thumb {
        id: String,
        path: std::path::PathBuf,
        bytes: u64,
    },
    ThumbFailed {
        id: String,
        message: String,
    },
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
    /// Die UI muss jetzt das Apple-ID-Passwort abfragen.
    PasswordRequired,
    /// Die UI muss jetzt einen 2FA-Code abfragen.
    TwoFactorRequired,
    Authenticated {
        trusted: bool,
    },
    /// So viele Datensätze sind bereits im Katalog.
    Progress {
        stored: u64,
    },
    /// So viele Vorschauen liegen bereits auf der Platte.
    Thumbnails {
        stored: u64,
    },
    /// Die Bibliothek ist unveraendert; dieser Lauf kostete einen Request.
    Unchanged,
    /// Gesamtbestand laut iCloud — Bezugsgroesse fuer Skelette und Fortschritt.
    LibraryCount {
        total: u64,
    },
    /// So viele in iCloud geloeschte Fotos wurden ausgetragen.
    Deleted {
        removed: u64,
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
    fn parses_password_required() {
        let line = r#"{"type":"password_required","username":"a@b.c"}"#;
        let msg: SidecarMessage = serde_json::from_str(line).unwrap();
        assert_eq!(
            msg,
            SidecarMessage::PasswordRequired {
                username: "a@b.c".into()
            }
        );
    }

    #[test]
    fn parses_change_tracking_lines() {
        let cases: Vec<(&str, SidecarMessage)> = vec![
            (
                r#"{"type":"cursor","token":"AQAAAF8="}"#,
                SidecarMessage::Cursor {
                    token: Some("AQAAAF8=".into()),
                },
            ),
            (
                r#"{"type":"cursor","token":null}"#,
                SidecarMessage::Cursor { token: None },
            ),
            (r#"{"type":"unchanged"}"#, SidecarMessage::Unchanged),
            (
                r#"{"type":"deleted","id":"abc"}"#,
                SidecarMessage::Deleted { id: "abc".into() },
            ),
            (
                r#"{"type":"offset","value":1200}"#,
                SidecarMessage::Offset { value: 1200 },
            ),
            (
                r#"{"type":"baseline_complete","count":17000}"#,
                SidecarMessage::BaselineComplete { count: 17000 },
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
    fn parses_unresolved_change() {
        let line = r#"{"type":"change_unresolved","id":"x","record_type":"CPLAlbum","message":"kein Asset"}"#;
        let msg: SidecarMessage = serde_json::from_str(line).unwrap();
        assert!(matches!(msg, SidecarMessage::ChangeUnresolved { .. }));
    }

    #[test]
    fn parses_thumb_lines() {
        let line = r#"{"type":"thumb","id":"a","path":"/cache/a.jpg","bytes":34567}"#;
        let msg: SidecarMessage = serde_json::from_str(line).unwrap();
        assert_eq!(
            msg,
            SidecarMessage::Thumb {
                id: "a".into(),
                path: std::path::PathBuf::from("/cache/a.jpg"),
                bytes: 34567,
            }
        );

        let line = r#"{"type":"thumb_failed","id":"a","message":"keine Vorschau"}"#;
        let msg: SidecarMessage = serde_json::from_str(line).unwrap();
        assert!(matches!(msg, SidecarMessage::ThumbFailed { .. }));
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
