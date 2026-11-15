use magi_core::{Command, Request, Response, ResponseData, SecretString};
use serde_json::json;

#[test]
fn request_envelope_preserves_id_and_snake_case_command() {
    let request = Request {
        id: "request-1".into(),
        command: Command::RetryCompletionSave {
            run_id: "run-1".into(),
        },
    };
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(
        wire,
        json!({"id":"request-1", "command":{"type":"retry_completion_save", "data":{"run_id":"run-1"}}})
    );
    let decoded: Request = serde_json::from_value(wire).unwrap();
    assert!(
        matches!(decoded.command, Command::RetryCompletionSave { run_id } if run_id == "run-1")
    );
}

#[test]
fn response_envelope_preserves_result_and_request_id() {
    let response = Response {
        id: "request-2".into(),
        result: Ok(ResponseData::Run {
            run_id: "run-2".into(),
        }),
    };
    let wire = serde_json::to_value(&response).unwrap();
    assert_eq!(
        wire,
        json!({"id":"request-2", "result":{"Ok":{"type":"run", "data":{"run_id":"run-2"}}}})
    );
    let decoded: Response = serde_json::from_value(wire).unwrap();
    assert!(matches!(decoded.result, Ok(ResponseData::Run { run_id }) if run_id == "run-2"));
    let error = Response {
        id: "request-3".into(),
        result: Err("unavailable".into()),
    };
    assert_eq!(
        serde_json::to_value(error).unwrap(),
        json!({"id":"request-3", "result":{"Err":"unavailable"}})
    );
}

#[test]
fn unit_command_and_response_use_the_existing_wire_tags() {
    assert_eq!(
        serde_json::to_value(Command::ListStorageFailures).unwrap(),
        json!({"type":"list_storage_failures"})
    );
    assert_eq!(
        serde_json::to_value(ResponseData::Empty).unwrap(),
        json!({"type":"empty"})
    );
    assert!(serde_json::from_value::<Command>(json!({"type":"unsupported"})).is_err());
}

#[test]
fn secret_is_transmitted_but_redacted_in_debug_output() {
    let request = Request {
        id: "request-4".into(),
        command: Command::SetProviderCredential {
            project_id: "project".into(),
            endpoint: "test-endpoint".into(),
            credential: SecretString("FAKE_ONLY".into()),
        },
    };
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["command"]["data"]["credential"], "FAKE_ONLY");
    let decoded: Request = serde_json::from_value(wire).unwrap();
    let debug = format!("{decoded:?}");
    assert!(debug.contains("[redacted]"));
    assert!(!debug.contains("FAKE_ONLY"));
}
