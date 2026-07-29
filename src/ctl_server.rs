//! Request/response control server on a Unix domain socket.
//!
//! This module only does I/O and serialization: it turns JSON lines into
//! [`CtlRequest`]s, hands them to the main loop over a channel and writes the
//! answer back. It never touches the application state; reading and mutating
//! `State` happens in `app.rs`.
//!
//! The wire protocol is specified in `docs/ctl-rpc/01-protocol.md`.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

/// Bumped only when the framing or the envelope changes. Adding methods,
/// fields or error codes is additive.
pub const PROTOCOL_VERSION: u32 = 1;

// Error codes are API. Do not rename them, do not reuse them for anything
// else.
pub const ERR_UNKNOWN_METHOD: &str = "unknown_method";
pub const ERR_INVALID_PARAMS: &str = "invalid_params";
pub const ERR_NO_MATCH: &str = "no_match";
pub const ERR_INTERNAL: &str = "internal";

/// How long a connection waits for the main loop before giving up.
const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Upper bound on the length of an error message echoed back to the client.
/// Serde errors quote the payload, which may hold environment variables.
const MAX_ERROR_MESSAGE: usize = 200;

pub const KNOWN_METHODS: &[&str] = &[
  "ls", "screen", "start", "stop", "restart", "kill", "shutdown",
];

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum CtlRequest {
  Ls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pattern: Option<String>,
  },
  Screen {
    name: String,
  },
  Start {
    pattern: String,
  },
  Stop {
    pattern: String,
  },
  Restart {
    pattern: String,
  },
  Kill {
    pattern: String,
  },
  Shutdown {},
}

#[derive(Debug)]
pub enum CtlResponse {
  Ok(Value),
  Err {
    code: &'static str,
    message: String,
  },
}

impl CtlResponse {
  pub fn err(code: &'static str, message: impl Into<String>) -> Self {
    CtlResponse::Err {
      code,
      message: message.into(),
    }
  }
}

/// What the ctl server sends to the main loop: a request and the channel the
/// answer has to come back on.
pub type CtlMessage = (CtlRequest, oneshot::Sender<CtlResponse>);

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RequestEnvelopeIn {
  #[serde(rename = "type")]
  typ: Option<String>,
  #[serde(default)]
  id: u64,
  method: String,
  #[serde(default)]
  params: Value,
}

#[derive(Serialize)]
struct RequestEnvelopeOut<'a> {
  #[serde(rename = "type")]
  typ: &'a str,
  id: u64,
  method: &'a str,
  params: Value,
}

#[derive(Serialize)]
struct ResponseEnvelope<'a> {
  #[serde(rename = "type")]
  typ: &'a str,
  id: u64,
  #[serde(skip_serializing_if = "Option::is_none")]
  result: Option<&'a Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  error: Option<ErrorObj<'a>>,
}

#[derive(Serialize)]
struct ErrorObj<'a> {
  code: &'a str,
  message: &'a str,
}

#[derive(Serialize)]
struct HelloEnvelope<'a> {
  #[serde(rename = "type")]
  typ: &'a str,
  protocol: u32,
  app: String,
  features: Vec<&'a str>,
}

/// Why a line could not be turned into a [`CtlRequest`].
#[derive(Debug, Eq, PartialEq)]
pub struct ParseError {
  pub id: u64,
  pub code: &'static str,
  pub message: String,
  /// A line that is not a well formed request envelope closes the connection;
  /// a well formed request the server cannot honour does not.
  pub fatal: bool,
}

fn truncate(s: &str) -> String {
  if s.chars().count() <= MAX_ERROR_MESSAGE {
    return s.to_string();
  }
  s.chars().take(MAX_ERROR_MESSAGE).collect::<String>() + "…"
}

/// Parses one line of the protocol into a request and its id.
pub fn parse_request(line: &str) -> Result<(u64, CtlRequest), ParseError> {
  let envelope: RequestEnvelopeIn =
    serde_json::from_str(line).map_err(|err| ParseError {
      id: 0,
      code: ERR_INVALID_PARAMS,
      message: truncate(&err.to_string()),
      fatal: true,
    })?;

  let id = envelope.id;

  if envelope.typ.as_deref() != Some("request") {
    return Err(ParseError {
      id,
      code: ERR_INVALID_PARAMS,
      message: "expected field `type` to be \"request\"".to_string(),
      fatal: true,
    });
  }

  if !KNOWN_METHODS.contains(&envelope.method.as_str()) {
    return Err(ParseError {
      id,
      code: ERR_UNKNOWN_METHOD,
      message: format!("unknown method '{}'", truncate(&envelope.method)),
      fatal: false,
    });
  }

  // `params` is optional in the protocol but the tagged enum always wants it.
  let params = if envelope.params.is_null() {
    Value::Object(Map::new())
  } else {
    envelope.params
  };

  let mut tagged = Map::new();
  tagged.insert("method".to_string(), Value::String(envelope.method));
  tagged.insert("params".to_string(), params);

  serde_json::from_value::<CtlRequest>(Value::Object(tagged))
    .map(|req| (id, req))
    .map_err(|err| ParseError {
      id,
      code: ERR_INVALID_PARAMS,
      message: truncate(&err.to_string()),
      fatal: false,
    })
}

/// Encodes a request the way a client has to send it. Used by the tests as the
/// executable documentation of the wire format.
pub fn request_to_wire(id: u64, req: &CtlRequest) -> String {
  let value = serde_json::to_value(req)
    .expect("CtlRequest is always serializable to a Value");
  let method = value
    .get("method")
    .and_then(Value::as_str)
    .expect("a serialized CtlRequest always carries its method")
    .to_string();
  let params = value
    .get("params")
    .cloned()
    .unwrap_or_else(|| Value::Object(Map::new()));

  serde_json::to_string(&RequestEnvelopeOut {
    typ: "request",
    id,
    method: &method,
    params,
  })
  .expect("the request envelope is always serializable")
}

/// Encodes a response.
pub fn response_to_wire(id: u64, response: &CtlResponse) -> String {
  let envelope = match response {
    CtlResponse::Ok(result) => ResponseEnvelope {
      typ: "response",
      id,
      result: Some(result),
      error: None,
    },
    CtlResponse::Err { code, message } => ResponseEnvelope {
      typ: "response",
      id,
      result: None,
      error: Some(ErrorObj { code, message }),
    },
  };
  serde_json::to_string(&envelope)
    .expect("the response envelope is always serializable")
}

/// The greeting sent as soon as a connection is accepted.
pub fn hello_line() -> String {
  serde_json::to_string(&HelloEnvelope {
    typ: "hello",
    protocol: PROTOCOL_VERSION,
    app: format!("monade-mprocs {}", env!("CARGO_PKG_VERSION")),
    features: Vec::new(),
  })
  .expect("the hello envelope is always serializable")
}

// ---------------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------------

/// Removes the socket file when the server goes away, on every exit path.
#[cfg(unix)]
struct SocketGuard {
  path: std::path::PathBuf,
}

#[cfg(unix)]
impl Drop for SocketGuard {
  fn drop(&mut self) {
    if let Err(err) = std::fs::remove_file(&self.path) {
      if err.kind() != std::io::ErrorKind::NotFound {
        log::warn!(
          "Failed to remove control socket {}: {}",
          self.path.display(),
          err
        );
      }
    }
  }
}

/// A bound control socket, not serving yet.
#[cfg(unix)]
pub struct CtlSocket {
  listener: tokio::net::UnixListener,
  guard: SocketGuard,
}

/// Binds the control socket, cleaning up an orphan socket file left behind by
/// a previous run. Fails when another live instance owns the path.
///
/// Must be called from inside a tokio runtime.
#[cfg(unix)]
pub fn bind_ctl_socket(path: &Path) -> anyhow::Result<CtlSocket> {
  use anyhow::Context;
  use std::os::unix::fs::PermissionsExt;

  if let Some(parent) = path.parent() {
    if !parent.as_os_str().is_empty() {
      std::fs::create_dir_all(parent).with_context(|| {
        format!("Failed to create directory {}", parent.display())
      })?;
    }
  }

  if std::fs::symlink_metadata(path).is_ok() {
    // Somebody answering means a live instance owns this path.
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
      anyhow::bail!(
        "Control socket {} is already in use by another running instance.",
        path.display()
      );
    }
    std::fs::remove_file(path).with_context(|| {
      format!("Failed to remove stale control socket {}", path.display())
    })?;
  }

  let listener = tokio::net::UnixListener::bind(path).with_context(|| {
    format!("Failed to bind control socket {}", path.display())
  })?;
  let guard = SocketGuard {
    path: path.to_path_buf(),
  };

  std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    .with_context(|| {
      format!("Failed to set permissions on {}", path.display())
    })?;

  Ok(CtlSocket { listener, guard })
}

#[cfg(not(unix))]
pub struct CtlSocket {}

#[cfg(not(unix))]
pub fn bind_ctl_socket(_path: &Path) -> anyhow::Result<CtlSocket> {
  anyhow::bail!("The control socket is only supported on unix platforms.")
}

/// Serves the control socket until `shutdown` fires.
#[cfg(unix)]
pub async fn ctl_server_main(
  socket: CtlSocket,
  tx: UnboundedSender<CtlMessage>,
  shutdown: triggered::Listener,
) -> anyhow::Result<()> {
  use futures::future::FutureExt;
  use futures::select;

  let CtlSocket { listener, guard } = socket;

  loop {
    let on_exit = shutdown.clone();
    let accepted = select! {
      _ = on_exit.fuse() => break,
      conn = listener.accept().fuse() => conn,
    };

    match accepted {
      Ok((stream, _addr)) => {
        let tx = tx.clone();
        tokio::spawn(async move {
          handle_conn(stream, tx).await;
        });
      }
      Err(err) => {
        log::warn!("Control socket accept failed: {}", err);
        break;
      }
    }
  }

  // Explicit, so that the socket file is gone before we return.
  drop(guard);

  Ok(())
}

#[cfg(not(unix))]
pub async fn ctl_server_main(
  _socket: CtlSocket,
  _tx: UnboundedSender<CtlMessage>,
  _shutdown: triggered::Listener,
) -> anyhow::Result<()> {
  Ok(())
}

#[cfg(unix)]
async fn handle_conn(
  stream: tokio::net::UnixStream,
  tx: UnboundedSender<CtlMessage>,
) {
  use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

  let (read_half, mut write_half) = stream.into_split();
  let mut reader = BufReader::new(read_half);

  async fn write_line<W: tokio::io::AsyncWrite + Unpin>(
    w: &mut W,
    line: &str,
  ) -> std::io::Result<()> {
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
  }

  if write_line(&mut write_half, &hello_line()).await.is_err() {
    return;
  }

  let mut line = String::new();
  loop {
    line.clear();
    match reader.read_line(&mut line).await {
      Ok(0) => break,
      Ok(_) => (),
      Err(err) => {
        log::warn!("Control connection read failed: {}", err);
        break;
      }
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
      continue;
    }

    let (id, req) = match parse_request(trimmed) {
      Ok(parsed) => parsed,
      Err(err) => {
        let response = CtlResponse::err(err.code, err.message);
        let _ = write_line(&mut write_half, &response_to_wire(err.id, &response))
          .await;
        if err.fatal {
          break;
        }
        continue;
      }
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    if tx.send((req, reply_tx)).is_err() {
      let response =
        CtlResponse::err(ERR_INTERNAL, "the application is shutting down");
      let _ =
        write_line(&mut write_half, &response_to_wire(id, &response)).await;
      break;
    }

    let response = match tokio::time::timeout(REPLY_TIMEOUT, reply_rx).await {
      Ok(Ok(response)) => response,
      Ok(Err(_)) => {
        CtlResponse::err(ERR_INTERNAL, "the main loop dropped the request")
      }
      Err(_) => CtlResponse::err(
        ERR_INTERNAL,
        "timed out waiting for the main loop to answer",
      ),
    };

    if write_line(&mut write_half, &response_to_wire(id, &response))
      .await
      .is_err()
    {
      break;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  /// Executable documentation of the wire format. A field renamed by accident
  /// fails here instead of breaking the clients in production.
  fn golden() -> Vec<(CtlRequest, &'static str)> {
    vec![
      (
        CtlRequest::Ls { pattern: None },
        r#"{"type":"request","id":1,"method":"ls","params":{}}"#,
      ),
      (
        CtlRequest::Ls {
          pattern: Some("web*".to_string()),
        },
        r#"{"type":"request","id":1,"method":"ls","params":{"pattern":"web*"}}"#,
      ),
      (
        CtlRequest::Screen {
          name: "api".to_string(),
        },
        r#"{"type":"request","id":1,"method":"screen","params":{"name":"api"}}"#,
      ),
      (
        CtlRequest::Start {
          pattern: "api".to_string(),
        },
        r#"{"type":"request","id":1,"method":"start","params":{"pattern":"api"}}"#,
      ),
      (
        CtlRequest::Stop {
          pattern: "*".to_string(),
        },
        r#"{"type":"request","id":1,"method":"stop","params":{"pattern":"*"}}"#,
      ),
      (
        CtlRequest::Restart {
          pattern: "api".to_string(),
        },
        r#"{"type":"request","id":1,"method":"restart","params":{"pattern":"api"}}"#,
      ),
      (
        CtlRequest::Kill {
          pattern: "*worker".to_string(),
        },
        r#"{"type":"request","id":1,"method":"kill","params":{"pattern":"*worker"}}"#,
      ),
      (
        CtlRequest::Shutdown {},
        r#"{"type":"request","id":1,"method":"shutdown","params":{}}"#,
      ),
    ]
  }

  #[test]
  fn golden_requests_encode_exactly() {
    for (req, expected) in golden() {
      assert_eq!(request_to_wire(1, &req), expected, "encoding {:?}", req);
    }
  }

  #[test]
  fn golden_requests_decode_exactly() {
    for (req, wire) in golden() {
      assert_eq!(parse_request(wire), Ok((1, req.clone())), "decoding {}", wire);
    }
  }

  #[test]
  fn params_may_be_absent_or_null() {
    for wire in [
      r#"{"type":"request","id":7,"method":"ls"}"#,
      r#"{"type":"request","id":7,"method":"ls","params":null}"#,
      r#"{"type":"request","id":7,"method":"ls","params":{}}"#,
    ] {
      assert_eq!(
        parse_request(wire),
        Ok((7, CtlRequest::Ls { pattern: None })),
        "{}",
        wire
      );
    }
    assert_eq!(
      parse_request(r#"{"type":"request","id":7,"method":"shutdown"}"#),
      Ok((7, CtlRequest::Shutdown {}))
    );
  }

  #[test]
  fn unknown_method_is_reported_and_not_fatal() {
    let err = parse_request(r#"{"type":"request","id":3,"method":"nope"}"#)
      .unwrap_err();
    assert_eq!(err.code, ERR_UNKNOWN_METHOD);
    assert_eq!(err.id, 3);
    assert!(!err.fatal);
  }

  #[test]
  fn wrong_params_are_invalid_params() {
    // `pattern` is required by `start`.
    let err = parse_request(r#"{"type":"request","id":4,"method":"start"}"#)
      .unwrap_err();
    assert_eq!(err.code, ERR_INVALID_PARAMS);
    assert_eq!(err.id, 4);
    assert!(!err.fatal);

    // Wrong type for `pattern`.
    let err = parse_request(
      r#"{"type":"request","id":5,"method":"start","params":{"pattern":42}}"#,
    )
    .unwrap_err();
    assert_eq!(err.code, ERR_INVALID_PARAMS);
    assert_eq!(err.id, 5);
  }

  #[test]
  fn malformed_lines_are_fatal_with_id_zero() {
    for wire in ["not json", "{", r#"{"type":"request"}"#] {
      let err = parse_request(wire).unwrap_err();
      assert_eq!(err.code, ERR_INVALID_PARAMS, "{}", wire);
      assert_eq!(err.id, 0, "{}", wire);
      assert!(err.fatal, "{}", wire);
    }

    // A well formed envelope with the wrong `type` keeps its id but still
    // closes the connection.
    let err =
      parse_request(r#"{"type":"response","id":9,"method":"ls"}"#).unwrap_err();
    assert_eq!(err.code, ERR_INVALID_PARAMS);
    assert_eq!(err.id, 9);
    assert!(err.fatal);
  }

  #[test]
  fn unknown_envelope_fields_are_ignored() {
    assert_eq!(
      parse_request(
        r#"{"type":"request","id":2,"method":"ls","params":{},"extra":true}"#
      ),
      Ok((2, CtlRequest::Ls { pattern: None }))
    );
  }

  #[test]
  fn responses_encode_exactly() {
    assert_eq!(
      response_to_wire(1, &CtlResponse::Ok(json!({ "matched": 2 }))),
      r#"{"type":"response","id":1,"result":{"matched":2}}"#
    );
    assert_eq!(
      response_to_wire(
        2,
        &CtlResponse::err(ERR_NO_MATCH, "no proc matches 'api'")
      ),
      r#"{"type":"response","id":2,"error":{"code":"no_match","message":"no proc matches 'api'"}}"#
    );
    assert_eq!(
      response_to_wire(3, &CtlResponse::Ok(json!({ "screen": Value::Null }))),
      r#"{"type":"response","id":3,"result":{"screen":null}}"#
    );
  }

  #[test]
  fn hello_announces_the_protocol_version() {
    let hello: Value = serde_json::from_str(&hello_line()).unwrap();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["protocol"], 1);
    assert!(hello["app"]
      .as_str()
      .unwrap()
      .starts_with("monade-mprocs "));
    assert_eq!(hello["features"], json!([]));
  }

  #[test]
  fn error_messages_are_truncated() {
    let payload = "x".repeat(5_000);
    let err =
      parse_request(&format!(r#"{{"type":"request","id":1,"method":"{}"}}"#, payload))
        .unwrap_err();
    assert!(err.message.chars().count() <= MAX_ERROR_MESSAGE + 20);
  }
}
