//! Integration tests: spawn leekscript-lsp, send LSP messages, assert responses.

use std::io::{Read, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};

/// LSP message framing: "Content-Length: N\r\n\r\n" + N bytes JSON.
fn send_lsp_message(stdin: &mut ChildStdin, msg: &str) {
    let bytes = msg.as_bytes();
    let header = format!("Content-Length: {}\r\n\r\n", bytes.len());
    stdin.write_all(header.as_bytes()).unwrap();
    stdin.write_all(bytes).unwrap();
    stdin.flush().unwrap();
}

fn read_lsp_message(stdout: &mut ChildStdout) -> Option<String> {
    let mut header = String::new();
    loop {
        let mut buf = [0u8; 1];
        if stdout.read(&mut buf).ok()? == 0 {
            return None;
        }
        let c = buf[0] as char;
        header.push(c);
        if header.ends_with("\r\n\r\n") {
            break;
        }
    }
    let len_str = header
        .trim_start_matches("Content-Length: ")
        .trim_end_matches("\r\n\r\n");
    let len: usize = len_str.parse().ok()?;
    let mut body = vec![0u8; len];
    stdout.read_exact(&mut body).ok()?;
    Some(String::from_utf8(body).ok()?)
}

#[test]
fn lsp_initialize_returns_capabilities() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_leekscript-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn leekscript-lsp");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    let init_request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":null,"rootUri":null,"capabilities":{},"clientInfo":{"name":"test","version":"0.1.0"}}}"#;
    send_lsp_message(&mut stdin, init_request);

    let response = read_lsp_message(&mut stdout).expect("read initialize response");
    let json: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(json["id"], 1);
    assert!(json["result"].is_object());
    let caps = &json["result"]["capabilities"];
    assert!(caps["hoverProvider"].is_object() || caps["hoverProvider"].as_bool() == Some(true));
    assert!(
        caps["textDocumentSync"].is_object() || caps["textDocumentSync"].is_number(),
        "textDocumentSync: {:?}",
        caps["textDocumentSync"]
    );

    child.kill().ok();
}

#[test]
fn lsp_did_open_and_hover_returns_type() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_leekscript-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn leekscript-lsp");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // Initialize
    let init_request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":null,"rootUri":null,"capabilities":{},"clientInfo":{"name":"test","version":"0.1.0"}}}"#;
    send_lsp_message(&mut stdin, init_request);
    let _ = read_lsp_message(&mut stdout);

    // Initialized notification
    send_lsp_message(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
    );

    // didOpen with simple script
    let uri = "file:///tmp/test.leek";
    let source = "var x = 1 + 2;";
    let did_open = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri,
                "languageId": "leekscript",
                "version": 1,
                "text": source
            }
        }
    });
    send_lsp_message(&mut stdin, &did_open.to_string());

    // Hover at "x" (line 0, character 4)
    let hover_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/hover",
        "params": {
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 4 }
        }
    });
    send_lsp_message(&mut stdin, &hover_request.to_string());

    // Read messages until we get the hover response (id: 2). We may get publish_diagnostics first.
    let mut hover_response = None;
    for _ in 0..5 {
        let msg = read_lsp_message(&mut stdout).expect("read message");
        let json: serde_json::Value = serde_json::from_str(&msg).unwrap();
        if json.get("id") == Some(&serde_json::json!(2)) {
            hover_response = Some(json);
            break;
        }
    }

    let response = hover_response.expect("hover response");
    let contents = response["result"]["contents"]
        .as_str()
        .or_else(|| response["result"]["contents"]["value"].as_str());
    assert!(
        contents.is_some(),
        "hover should return contents: {:?}",
        response["result"]
    );
    let content = contents.unwrap();
    assert!(
        content.contains("integer") || content.contains("any"),
        "hover content should show type: {}",
        content
    );

    child.kill().ok();
}
