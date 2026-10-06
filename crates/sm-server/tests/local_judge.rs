//! Exercise the real daemon and its protocol, without loading model weights.
#[test]
fn local_judge_service_contract() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = std::process::Command::new("python3")
        .arg(root.join("scripts/local-judge/tests/test_service.py"))
        .arg("-v")
        .output()
        .expect("run judge protocol tests");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
