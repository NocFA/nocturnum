//! The `nocturnum` binary itself: config checks, startup and shutdown.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

fn nocturnum() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_nocturnum"));
    for var in [
        "NOCTURNUM_CONFIG",
        "NOCTURNUM_HOST",
        "NOCTURNUM_PORT",
        "NOCTURNUM_DATA",
        "NOCTURNUM_SNAPSHOTS",
        "NOCTURNUM_ALLOW_ANONYMOUS",
        "NOCTURNUM_ADMIN_KEY",
        "NOCTURNUM_READ_ONLY_KEY",
        "NOCTURNUM_LOG",
    ] {
        c.env_remove(var);
    }
    c
}

#[test]
fn refuses_a_public_bind_without_keys() {
    let out = nocturnum().args(["--host", "0.0.0.0"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no API keys"), "{err}");
}

#[test]
fn print_config_hides_keys() {
    let out = nocturnum()
        .arg("--print-config")
        .env("NOCTURNUM_ADMIN_KEY", "example-admin-key-12")
        .env("NOCTURNUM_PORT", "7777")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("port = 7777"), "{text}");
    assert!(text.contains("admin_key = \"(set)\""), "{text}");
    assert!(!text.contains("example-admin-key-12"));
}

#[test]
fn serves_and_stops_cleanly_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = nocturnum()
        .args(["--port", "0", "--data"])
        .arg(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let addr = loop {
        let line = lines
            .next()
            .expect("the server printed its address")
            .unwrap();
        if let Some(rest) = line.split("listening on http://").nth(1) {
            break rest.trim().to_owned();
        }
    };
    let mut stream = TcpStream::connect(&addr).unwrap();
    write!(
        stream,
        "GET /livez HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");

    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let exit = child.wait().unwrap();
    assert!(exit.success(), "{exit:?}");
    assert!(lines.any(|l| l.unwrap().contains("shutting down")));
}
