// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The agent protocol's wire format.

use hatchrun_workflow::agent::{AgentEvent, BootDocument, CiBoot};

#[test]
fn events_round_trip_with_a_type_tag() {
    let e = AgentEvent::Line { text: "hi".into() };
    let json = r#"{"type":"line","text":"hi"}"#;
    assert_eq!(serde_json::to_string(&e).unwrap(), json);
    assert_eq!(serde_json::from_str::<AgentEvent>(json).unwrap(), e);
}

#[test]
fn boot_document_debug_hides_the_token() {
    let d = BootDocument {
        ci: CiBoot {
            url: "https://ci".into(),
            token: "secret".into(),
        },
    };
    assert!(!format!("{d:?}").contains("secret"));
}
