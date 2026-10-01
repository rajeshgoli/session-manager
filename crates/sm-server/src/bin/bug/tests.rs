use super::*;

#[test]
fn id_takes_the_prefix_or_not() {
    assert_eq!(
        normalize_id("BR-20261001-041502-3fa9c1"),
        "BR-20261001-041502-3fa9c1"
    );
    assert_eq!(
        normalize_id(" 20261001-041502-3fa9c1 "),
        "BR-20261001-041502-3fa9c1"
    );
}

#[test]
fn show_prints_text_metadata_and_json_then_saves_the_screenshot() {
    let report = json!({
        "bug_id": "BR-1", "created_at": "2026-10-01T04:15:02Z", "client": "web",
        "client_version": "b3f9", "page": "Board", "route": "/board?lane=3",
        "text": "Board shows all parts done\nmore",
        "issue": {"repo": "acme/widgets", "number": 1870, "url": "https://github.com/acme/widgets/issues/1870"},
        "has_screenshot": true,
        "page_data": {"/client/board": {"lanes": []}},
        "server_facts": {"sessions": []},
    });
    let lines = show_lines(&report);
    assert_eq!(lines[0], "Board shows all parts done\nmore");
    assert!(lines.contains(
        &"Issue    acme/widgets#1870 https://github.com/acme/widgets/issues/1870".to_owned()
    ));
    assert!(lines.contains(&"Route    /board?lane=3".to_owned()));
    let text = lines.join("\n");
    assert!(
        text.contains("Page data:\n{\n  \"/client/board\""),
        "{text}"
    );
    assert!(
        text.contains("Server facts:\n{\n  \"sessions\": []\n}"),
        "{text}"
    );

    let dir = env::temp_dir().join(format!("sm-bug-show-{}", process::id()));
    fs::create_dir_all(&dir).unwrap();
    let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
    let path = write_screenshot(&dir, "BR-1", &STANDARD.encode(png)).unwrap();
    assert_eq!(path, dir.join("sm-bug-BR-1.png"));
    assert_eq!(fs::read(&path).unwrap(), png);
    assert!(write_screenshot(&dir, "BR-2", "not base64!").is_err());
    fs::remove_dir_all(dir).unwrap();
}
