//! The `--convert-page` child protocol of web_fetch: the URL on the first
//! line of stdin, the document after it; `full` or `cut` and the text on
//! stdout.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn convert(kind: &str, input: &[u8]) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_havn-gateway"))
        .args(["--convert-page", kind])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn html_is_converted_with_links_made_absolute() {
    let (ok, out, err) = convert(
        "html",
        b"https://example.com/docs/\n<h1>Title</h1><p>See <a href=\"page\">this</a>.</p>",
    );
    assert!(ok, "{err}");
    let (flag, text) = out.split_once('\n').expect("flag line");
    assert_eq!(flag, "full");
    assert!(text.contains("# Title"), "{text}");
    assert!(text.contains("(https://example.com/docs/page)"), "{text}");
}

#[test]
fn bad_input_fails_with_a_reason() {
    let (ok, _, err) = convert("pdf", b"https://example.com/a.pdf\nnot a pdf");
    assert!(!ok);
    assert!(err.contains("PDF"), "{err}");
    let (ok, _, err) = convert("html", b"no url line");
    assert!(!ok);
    assert!(err.contains("URL"), "{err}");
    let (ok, _, _) = convert("exe", b"https://example.com/\nx");
    assert!(!ok);
}
