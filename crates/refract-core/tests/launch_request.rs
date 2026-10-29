//! The golden launch request in `tests/fixtures/launch_request_v1.json`, which the
//! VS Code extension's tests read too.

use refract_core::launch::{launch_argv, launch_title};
use refract_core::protocol::{LaunchMode, LaunchRequest, LAUNCH_REQUEST_VERSION};
use serde_json::Value;

const SESSION_ID: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d8e";
const PROMPT: &str = "This repo has uncommitted changes.\tReview `git diff`.";

fn fixture() -> String {
    let path = format!(
        "{}/tests/fixtures/launch_request_v1.json",
        env!("CARGO_MANIFEST_DIR")
    );
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
    };
    assert_eq!(request.argv, launch_argv(&mode, SESSION_ID));
    assert_eq!(request.title, launch_title(&mode));

    let written = serde_json::to_value(&request).unwrap();
    assert_eq!(written, serde_json::from_str::<Value>(&text).unwrap());
}
