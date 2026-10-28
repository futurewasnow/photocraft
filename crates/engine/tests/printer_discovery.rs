//! Compile the same bounded worker with synthetic backends; no printer is contacted.
use photocraft_engine::{EngineError, Result};

#[path = "../src/print_cmds/printer_discovery.rs"]
mod printer_discovery;

#[test]
fn both_discovery_routes_preserve_success_and_error_results() {
    use printer_discovery::{Kind, run};
    use serde_json::json;
    assert_eq!(run(Kind::System, || Ok(json!({"printers":[{"name":"Synthetic"}]}))).unwrap()["printers"][0]["name"], "Synthetic");
    assert!(run(Kind::Rgb, || Err(EngineError::Other("synthetic failure".into()))).unwrap_err().to_string().contains("synthetic failure"));
}
