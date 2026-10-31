//! The golden launch requests in `tests/fixtures/launch_request_v1.json` and
//! `launch_request_focus_v1.json`, which the VS Code extension's tests read too.

use refract_core::launch::{launch_argv, launch_title};
use refract_core::protocol::{FocusTarget, LaunchMode, LaunchRequest, LAUNCH_REQUEST_VERSION};
use serde_json::Value;

const SESSION_ID: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d8e";
const PROMPT: &str = "This repo has uncommitted changes.\tReview `git diff`.";

fn fixture() -> String {
    read_fixture("launch_request_v1.json")
}

fn read_fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn golden_request_parses() {
    let text = fixture();
    let request: LaunchRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(request.version, LAUNCH_REQUEST_VERSION);
    assert_eq!(request.request_id, "6f1c2a7e-1b8d-4c3e-9a5f-2d4b6e8f0a1c");
    assert_eq!(request.folder, "/home/example/projects/demo-repo");
    assert_eq!(request.title, "New agent");
    assert_eq!(
        request.created_at,
        "2026-10-29T17:54:44Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap()
    );

    let mode = LaunchMode::New {
        prompt: Some(PROMPT.into()),
        plan_mode: false,
    };
    assert_eq!(request.argv, launch_argv(&mode, SESSION_ID));
    assert_eq!(request.title, launch_title(&mode));

    let written = serde_json::to_value(&request).unwrap();
    assert_eq!(written, serde_json::from_str::<Value>(&text).unwrap());
}

#[test]
fn golden_focus_request_is_what_the_writer_produces() {
    let text = read_fixture("launch_request_focus_v1.json");
    let mode = LaunchMode::Focus {
        session_id: SESSION_ID.into(),
    };
    let written = LaunchRequest {
        version: LAUNCH_REQUEST_VERSION,
        request_id: "6f1c2a7e-1b8d-4c3e-9a5f-2d4b6e8f0a1d".into(),
        folder: "/home/example/projects/demo-repo".into(),
        argv: launch_argv(&mode, SESSION_ID),
        title: launch_title(&mode).into(),
        created_at: "2026-10-29T17:54:44Z".parse().unwrap(),
        focus: Some(FocusTarget {
            session_id: SESSION_ID.into(),
            pids: vec![8123, 8090, 7311],
        }),
    };
    assert_eq!(
        serde_json::to_value(&written).unwrap(),
        serde_json::from_str::<Value>(&text).unwrap()
    );
    let parsed: LaunchRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed, written);
}
