use std::path::Path;

use ssync_adapters::adapter_for;

#[test]
fn omp_adapter_identifies_session_from_path() {
    let root = Path::new("/home/simon/.omp/agent/sessions");
    let adapter = adapter_for("omp", root).unwrap();
    assert_eq!(adapter.agent(), "omp");

    // omp encodes cwds under $HOME as `-<relative-path>` (shorter than pi's
    // `--<abs>--` form); identity logic must not care.
    let path = root
        .join("-Projects-ssync")
        .join("2026-07-02T08-15-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");

    let id = adapter.identify(&path).unwrap();
    assert_eq!(id.session_id, "019e539d-f6ab-71ac-be20-d3ae2b23ea4a");
    assert_eq!(id.project_id, "-Projects-ssync");
    assert!(adapter.is_session_file(&path));
    assert!(adapter.append_only());
}

#[test]
fn pi_adapter_via_factory() {
    let root = Path::new("/home/simon/.pi/agent/sessions");
    let adapter = adapter_for("pi", root).unwrap();
    assert_eq!(adapter.agent(), "pi");
}

#[test]
fn unknown_agent_is_rejected() {
    let err = adapter_for("clippy-9000", Path::new("/tmp")).unwrap_err();
    assert!(err.to_string().contains("clippy-9000"));
}
