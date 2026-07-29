//! End to end test of the control socket.
//!
//! It drives the real `server_main` and `ctl_server_main`, with a fake client
//! standing in for the TUI: the client is only a pair of channels, so nothing
//! here needs a terminal. The alternative — a fake backend for the whole
//! application — would test something other than the code that ships.

#![cfg(all(test, unix))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::app::server_main;
use crate::ctl_server::{
  bind_ctl_socket, ctl_server_main, request_to_wire, CtlMessage, CtlRequest,
};
use crate::protocol::{CltToSrv, SrvToClt};
use crate::{load_config, RunOptions};

const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// A throwaway directory, removed when the test ends.
struct TempDir {
  path: PathBuf,
}

impl TempDir {
  fn new(tag: &str) -> Self {
    let nanos = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let path = std::env::temp_dir()
      .join(format!("mprocs-{}-{}-{}", tag, std::process::id(), nanos));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
  }
}

impl Drop for TempDir {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

struct Conn {
  read: BufReader<OwnedReadHalf>,
  write: OwnedWriteHalf,
}

impl Conn {
  async fn open(socket: &Path) -> Self {
    let stream = timeout(IO_TIMEOUT, UnixStream::connect(socket))
      .await
      .expect("timed out connecting to the control socket")
      .expect("failed to connect to the control socket");
    let (read, write) = stream.into_split();
    Conn {
      read: BufReader::new(read),
      write,
    }
  }

  async fn read_line(&mut self) -> String {
    let mut line = String::new();
    let read = timeout(IO_TIMEOUT, self.read.read_line(&mut line))
      .await
      .expect("timed out reading from the control socket")
      .expect("failed to read from the control socket");
    assert!(read > 0, "the control server closed the connection");
    line
  }

  async fn read_json(&mut self) -> Value {
    let line = self.read_line().await;
    serde_json::from_str(&line)
      .unwrap_or_else(|err| panic!("not JSON: {:?} ({})", line, err))
  }

  async fn send_raw(&mut self, line: &str) {
    self.write.write_all(line.as_bytes()).await.unwrap();
    self.write.write_all(b"\n").await.unwrap();
    self.write.flush().await.unwrap();
  }

  async fn request(&mut self, id: u64, req: CtlRequest) -> Value {
    self.send_raw(&request_to_wire(id, &req)).await;
    let response = self.read_json().await;
    assert_eq!(response["type"], "response");
    assert_eq!(response["id"], id);
    response
  }

  /// The `result` of a request that has to succeed.
  async fn result(&mut self, id: u64, req: CtlRequest) -> Value {
    let response = self.request(id, req).await;
    assert!(
      response.get("error").is_none(),
      "unexpected error: {}",
      response
    );
    response["result"].clone()
  }
}

fn find_proc<'a>(procs: &'a Value, name: &str) -> &'a Value {
  procs
    .as_array()
    .expect("`procs` is an array")
    .iter()
    .find(|proc| proc["name"] == name)
    .unwrap_or_else(|| panic!("no proc named {} in {}", name, procs))
}

/// Polls `f` until it returns true, or fails after `IO_TIMEOUT`.
async fn wait_until<F: FnMut() -> bool>(what: &str, mut f: F) {
  let deadline = std::time::Instant::now() + IO_TIMEOUT;
  while std::time::Instant::now() < deadline {
    if f() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
  panic!("timed out waiting for {}", what);
}

#[tokio::test]
async fn ctl_socket_serves_the_stack_and_cleans_up() {
  let dir = TempDir::new("ctl-test");
  let yaml_path = dir.path.join("mprocs.yaml");
  let socket_path = dir.path.join("ctl.sock");
  let log_dir = dir.path.join("logs");

  std::fs::write(
    &yaml_path,
    "procs:\n  \
       sleeper:\n    \
         shell: \"echo hello-from-test; sleep 60\"\n  \
       echoer:\n    \
         shell: \"echo hi\"\n    \
         autostart: false\n",
  )
  .unwrap();

  let (config, keymap) = load_config(&RunOptions {
    yaml_path: yaml_path.clone(),
    ctl_socket: Some(socket_path.clone()),
    log_dir: Some(log_dir.clone()),
    ..Default::default()
  })
  .unwrap();

  let socket = bind_ctl_socket(&socket_path).unwrap();
  let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel::<CtlMessage>();
  let (clt_tx, srv_rx) = tokio::sync::mpsc::channel::<CltToSrv>(64);
  let (srv_tx, mut clt_rx) = tokio::sync::mpsc::unbounded_channel::<SrvToClt>();
  let (exit_trigger, exit_listener) = triggered::trigger();

  // The fake client: send the init message the server waits for, then drain
  // the draw commands so the unbounded channel does not grow forever.
  clt_tx
    .send(CltToSrv::Init {
      width: 80,
      height: 24,
    })
    .await
    .unwrap();
  let client = tokio::spawn(async move {
    while let Some(msg) = clt_rx.recv().await {
      if matches!(msg, SrvToClt::Quit) {
        break;
      }
    }
  });

  let server = tokio::spawn(async move {
    server_main(config, keymap, srv_tx, srv_rx, ctl_rx).await
  });
  let ctl_server = tokio::spawn(async move {
    ctl_server_main(socket, ctl_tx, exit_listener).await
  });

  let mut conn = Conn::open(&socket_path).await;

  // The greeting comes before anything else.
  let hello = conn.read_json().await;
  assert_eq!(hello["type"], "hello");
  assert_eq!(hello["protocol"], 1);

  // ---- ls -------------------------------------------------------------
  let procs = conn.result(1, CtlRequest::Ls { pattern: None }).await["procs"]
    .clone();
  assert_eq!(procs.as_array().unwrap().len(), 2, "{}", procs);

  let sleeper = find_proc(&procs, "sleeper");
  assert_eq!(sleeper["state"], "running");
  assert!(sleeper["pid"].as_u64().unwrap() > 0);
  assert!(sleeper["started_at"].as_u64().unwrap() > 0);
  assert_eq!(
    sleeper["log_file"],
    Value::from(log_dir.join("sleeper.log").to_string_lossy())
  );

  let echoer = find_proc(&procs, "echoer");
  assert_eq!(echoer["state"], "idle", "autostart: false stays idle");
  assert!(echoer.get("pid").is_none(), "an idle proc has no pid");

  // A pattern narrows the answer down.
  let procs = conn
    .result(
      2,
      CtlRequest::Ls {
        pattern: Some("sleep*".to_string()),
      },
    )
    .await["procs"]
    .clone();
  assert_eq!(procs.as_array().unwrap().len(), 1);
  assert_eq!(procs[0]["name"], "sleeper");

  // ---- screen ---------------------------------------------------------
  // The process writes as soon as it starts, but "as soon as" is another
  // thread: poll instead of sleeping.
  let mut screen = Value::Null;
  let deadline = std::time::Instant::now() + IO_TIMEOUT;
  while std::time::Instant::now() < deadline {
    screen = conn
      .result(
        3,
        CtlRequest::Screen {
          name: "sleeper".to_string(),
        },
      )
      .await["screen"]
      .clone();
    if screen.as_str().map_or(false, |s| s.contains("hello-from-test")) {
      break;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
  assert!(
    screen.as_str().unwrap().contains("hello-from-test"),
    "screen was {:?}",
    screen
  );

  let error = conn.request(
    4,
    CtlRequest::Screen {
      name: "nope".to_string(),
    },
  )
  .await;
  assert_eq!(error["error"]["code"], "no_match");

  // An idle proc has no vt to read.
  let screen = conn
    .result(
      5,
      CtlRequest::Screen {
        name: "echoer".to_string(),
      },
    )
    .await["screen"]
    .clone();
  assert_eq!(screen, Value::Null);

  // ---- start ----------------------------------------------------------
  let result = conn
    .result(
      6,
      CtlRequest::Start {
        pattern: "echo*".to_string(),
      },
    )
    .await;
  assert_eq!(result["matched"], 1);

  // Nothing matching is not an error.
  let result = conn
    .result(
      7,
      CtlRequest::Start {
        pattern: "ghost".to_string(),
      },
    )
    .await;
  assert_eq!(result["matched"], 0);

  // `echo hi` ends on its own: the exit status has to show up in `ls`.
  let mut echoer = Value::Null;
  let deadline = std::time::Instant::now() + IO_TIMEOUT;
  while std::time::Instant::now() < deadline {
    let procs = conn.result(8, CtlRequest::Ls { pattern: None }).await["procs"]
      .clone();
    echoer = find_proc(&procs, "echoer").clone();
    if echoer["state"] == "exited" {
      break;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
  assert_eq!(echoer["state"], "exited", "{}", echoer);
  assert_eq!(echoer["exit_code"], 0, "{}", echoer);
  assert_eq!(echoer["signal"], Value::Null, "{}", echoer);
  assert!(echoer.get("pid").is_none(), "an exited proc has no pid");

  // ---- unknown method -------------------------------------------------
  conn
    .send_raw(r#"{"type":"request","id":10,"method":"teleport"}"#)
    .await;
  let response = conn.read_json().await;
  assert_eq!(response["id"], 10);
  assert_eq!(response["error"]["code"], "unknown_method");

  // ---- the log file -----------------------------------------------------
  let log_file = log_dir.join("sleeper.log");
  wait_until("the log file to hold the process output", || {
    std::fs::read_to_string(&log_file)
      .map_or(false, |body| body.contains("hello-from-test"))
  })
  .await;
  let body = std::fs::read_to_string(&log_file).unwrap();
  assert!(
    body.contains("=== mprocs: sleeper started, pid="),
    "missing run marker in {:?}",
    body
  );

  // ---- shutdown ---------------------------------------------------------
  let result = conn.result(11, CtlRequest::Shutdown {}).await;
  assert_eq!(result, serde_json::json!({}));

  let server_result = timeout(IO_TIMEOUT, server)
    .await
    .expect("the app did not shut down")
    .expect("the server task panicked");
  server_result.expect("the server returned an error");

  exit_trigger.trigger();
  timeout(IO_TIMEOUT, ctl_server)
    .await
    .expect("the ctl server did not stop")
    .expect("the ctl server task panicked")
    .expect("the ctl server returned an error");
  let _ = timeout(IO_TIMEOUT, client).await;

  assert!(
    !socket_path.exists(),
    "the socket file outlived the application"
  );
}

#[tokio::test]
async fn a_stale_socket_is_reused_and_a_live_one_is_refused() {
  let dir = TempDir::new("ctl-bind");
  let socket_path = dir.path.join("ctl.sock");

  // An orphan file, of the kind left behind by a SIGKILL.
  std::fs::write(&socket_path, b"stale").unwrap();
  let socket = bind_ctl_socket(&socket_path).unwrap();
  assert!(socket_path.exists());

  // A second instance on the same path must refuse to start.
  let (ctl_tx, _ctl_rx) = tokio::sync::mpsc::unbounded_channel::<CtlMessage>();
  let (exit_trigger, exit_listener) = triggered::trigger();
  let serving = tokio::spawn(async move {
    ctl_server_main(socket, ctl_tx, exit_listener).await
  });

  let err = match bind_ctl_socket(&socket_path) {
    Ok(_) => panic!("binding a socket owned by a live instance must fail"),
    Err(err) => err,
  };
  assert!(
    err.to_string().contains("already in use"),
    "unexpected error: {}",
    err
  );

  exit_trigger.trigger();
  timeout(IO_TIMEOUT, serving)
    .await
    .expect("the ctl server did not stop")
    .unwrap()
    .unwrap();

  assert!(!socket_path.exists(), "the socket file was not removed");
}
