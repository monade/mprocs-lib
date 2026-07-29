use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, spawn};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use assert_matches::assert_matches;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use portable_pty::MasterPty;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::spawn_blocking;
use tui::layout::Rect;
use vt100::MouseProtocolMode;

use crate::config::{Config, ProcConfig};
use crate::encode_term::{encode_key, encode_mouse_event, KeyCodeEncodeModes};
use crate::error::ResultLogger;
use crate::key::Key;

pub struct Inst {
  pub vt: VtWrap,

  pub pid: u32,
  pub master: Box<dyn MasterPty + Send>,
  pub killer: Box<dyn ChildKiller + Send + Sync>,

  pub running: Arc<AtomicBool>,

  /// When this instance was spawned.
  pub started_at: SystemTime,
}

/// Where and how a process tees its pty output.
#[derive(Clone, Debug)]
pub struct LogFileConfig {
  pub path: PathBuf,
  pub max_bytes: u64,
}

fn epoch_secs(time: SystemTime) -> u64 {
  time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Opens the log file of a process in append mode, truncating it first when it
/// grew past `max_bytes`, and writes the marker separating this run from the
/// previous ones. Returns `None` (after a warning) if anything goes wrong: a
/// full disk must not take down the process nor the TUI.
fn open_log_file(
  cfg: &LogFileConfig,
  proc_name: &str,
  pid: u32,
  started_at: SystemTime,
) -> Option<File> {
  fn open(cfg: &LogFileConfig) -> std::io::Result<File> {
    if let Some(parent) = cfg.path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    let too_big = std::fs::metadata(&cfg.path)
      .map_or(false, |meta| meta.len() > cfg.max_bytes);
    let mut opts = OpenOptions::new();
    opts.create(true).write(true);
    if too_big {
      opts.truncate(true);
    } else {
      opts.append(true);
    }
    opts.open(&cfg.path)
  }

  match open(cfg) {
    Ok(mut file) => {
      let marker = format!(
        "\n=== mprocs: {} started, pid={}, at={} ===\n",
        proc_name,
        pid,
        epoch_secs(started_at)
      );
      if let Err(err) = file.write_all(marker.as_bytes()) {
        log::warn!(
          "Failed to write to log file {}: {}",
          cfg.path.display(),
          err
        );
        return None;
      }
      Some(file)
    }
    Err(err) => {
      log::warn!("Failed to open log file {}: {}", cfg.path.display(), err);
      None
    }
  }
}

impl Debug for Inst {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Inst")
      .field("pid", &self.pid)
      .field("running", &self.running)
      .finish()
  }
}

pub type VtWrap = Arc<RwLock<vt100::Parser>>;

impl Inst {
  fn spawn(
    id: usize,
    cmd: CommandBuilder,
    tx: UnboundedSender<(usize, ProcUpdate)>,
    size: &Size,
    name: &str,
    log: Option<&LogFileConfig>,
  ) -> anyhow::Result<Self> {
    let vt = vt100::Parser::new(size.height, size.width, 1000);
    let vt = Arc::new(RwLock::new(vt));

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
      rows: size.height,
      cols: size.width,
      pixel_width: 0,
      pixel_height: 0,
    })?;

    let running = Arc::new(AtomicBool::new(true));
    let mut child = pair.slave.spawn_command(cmd)?;
    let started_at = SystemTime::now();
    let pid = child.process_id().unwrap_or(0);
    let killer = child.clone_killer();

    let mut reader = pair.master.try_clone_reader().unwrap();

    let log_file =
      log.and_then(|cfg| open_log_file(cfg, name, pid, started_at));
    let log_path = log.map(|cfg| cfg.path.clone());

    {
      let tx = tx.clone();
      let vt = vt.clone();
      let running = running.clone();
      let mut log_file = log_file;
      spawn_blocking(move || {
        let mut buf = [0; 4 * 1024];
        loop {
          if !running.load(Ordering::Relaxed) {
            break;
          }

          match reader.read(&mut buf[..]) {
            Ok(count) => {
              if count > 0 {
                // Tee the raw bytes, escape sequences included: stripping
                // them here would lose information and needs a parser. The
                // consumer of the log file does the stripping.
                if let Some(file) = &mut log_file {
                  if let Err(err) = file.write_all(&buf[..count]) {
                    log::warn!(
                      "Failed to write to log file {}: {}. Log disabled for this run.",
                      log_path.as_deref().unwrap_or(Path::new("?")).display(),
                      err
                    );
                    log_file = None;
                  }
                }
                if let Ok(mut vt) = vt.write() {
                  vt.process(&buf[..count]);
                  match tx.send((id, ProcUpdate::Render)) {
                    Ok(_) => (),
                    Err(_) => break,
                  }
                }
              } else {
                thread::sleep(Duration::from_millis(10));
              }
            }
            _ => break,
          }
        }
      });
    }

    {
      let tx = tx.clone();
      let running = running.clone();
      spawn(move || {
        // Block until program exits
        let status = child.wait();
        running.store(false, Ordering::Relaxed);
        let (exit_code, signal) = match &status {
          Ok(status) => (
            Some(status.exit_code() as i32),
            status.signal().map(|s| s.to_string()),
          ),
          Err(_) => (None, None),
        };
        let _result = tx.send((id, ProcUpdate::Stopped { exit_code, signal }));
      });
    }

    let inst = Inst {
      vt,

      pid,
      master: pair.master,
      killer,

      running,

      started_at,
    };
    Ok(inst)
  }

  fn resize(&self, size: &Size) {
    let rows = size.height;
    let cols = size.width;

    self
      .master
      .resize(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
      })
      .log_ignore();

    if let Ok(mut vt) = self.vt.write() {
      vt.set_size(rows, cols);
    }
  }
}

pub struct Proc {
  pub id: usize,
  pub name: String,
  pub to_restart: bool,
  pub changed: bool,
  pub cmd: CommandBuilder,
  size: Size,

  stop_signal: StopSignal,

  pub tx: UnboundedSender<(usize, ProcUpdate)>,

  pub inst: ProcState,
  pub copy_mode: CopyMode,

  /// Exit code of the last finished run, if it is known.
  pub last_exit_code: Option<i32>,
  /// Name of the signal that ended the last run, if any.
  pub last_signal: Option<String>,

  /// Directory of the log files, when file logging is enabled.
  log_dir: Option<PathBuf>,
  log_max_bytes: u64,
  /// Log file of this process. Recomputed on rename.
  pub log_file: Option<PathBuf>,
}

static NEXT_PROC_ID: AtomicUsize = AtomicUsize::new(1);

#[derive(Debug)]
pub enum ProcState {
  None,
  Some(Inst),
  Error(String),
}

#[derive(Debug)]
pub enum ProcUpdate {
  Render,
  Stopped {
    exit_code: Option<i32>,
    signal: Option<String>,
  },
  Started,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopSignal {
  #[serde(rename = "SIGINT")]
  SIGINT,
  #[serde(rename = "SIGTERM")]
  SIGTERM,
  #[serde(rename = "SIGKILL")]
  SIGKILL,
  SendKeys(Vec<Key>),
  HardKill,
}

impl Default for StopSignal {
  fn default() -> Self {
    StopSignal::SIGTERM
  }
}

impl Proc {
  pub fn new(
    name: String,
    cfg: &ProcConfig,
    tx: UnboundedSender<(usize, ProcUpdate)>,
    size: Rect,
    log_dir: Option<&Path>,
    log_max_bytes: u64,
  ) -> Self {
    let id = NEXT_PROC_ID.fetch_add(1, Ordering::Relaxed);
    let size = Size::new(size);
    let log_dir = log_dir.map(|dir| dir.to_path_buf());
    let log_file =
      log_dir.as_ref().map(|dir| crate::config::log_file_path(dir, &name));
    let mut proc = Proc {
      id,
      name,
      to_restart: false,
      changed: false,
      cmd: cfg.into(),
      size,

      stop_signal: cfg.stop.clone(),

      tx,

      inst: ProcState::None,
      copy_mode: CopyMode::None(None),

      last_exit_code: None,
      last_signal: None,

      log_dir,
      log_max_bytes,
      log_file,
    };

    if cfg.autostart {
      proc.spawn_new_inst();
    }

    proc
  }

  fn log_config(&self) -> Option<LogFileConfig> {
    self.log_file.as_ref().map(|path| LogFileConfig {
      path: path.clone(),
      max_bytes: self.log_max_bytes,
    })
  }

  fn spawn_new_inst(&mut self) {
    assert_matches!(self.inst, ProcState::None);

    let log = self.log_config();
    let spawned = Inst::spawn(
      self.id,
      self.cmd.clone(),
      self.tx.clone(),
      &self.size,
      &self.name,
      log.as_ref(),
    );
    let inst = match spawned {
      Ok(inst) => ProcState::Some(inst),
      Err(err) => ProcState::Error(err.to_string()),
    };
    self.inst = inst;
  }

  pub fn start(&mut self) {
    if !self.is_up() {
      self.inst = ProcState::None;
      self.last_exit_code = None;
      self.last_signal = None;
      self.spawn_new_inst();

      let _res = self.tx.send((self.id, ProcUpdate::Started));
    }
  }

  pub fn is_up(&self) -> bool {
    if let ProcState::Some(inst) = &self.inst {
      inst.running.load(Ordering::Relaxed)
    } else {
      false
    }
  }

  pub fn lock_vt(
    &self,
  ) -> Option<std::sync::RwLockReadGuard<'_, vt100::Parser>> {
    match &self.inst {
      ProcState::None => None,
      ProcState::Some(inst) => inst.vt.read().ok(),
      ProcState::Error(_) => None,
    }
  }

  pub fn lock_vt_mut(
    &mut self,
  ) -> Option<std::sync::RwLockWriteGuard<'_, vt100::Parser>> {
    match &self.inst {
      ProcState::None => None,
      ProcState::Some(inst) => inst.vt.write().ok(),
      ProcState::Error(_) => None,
    }
  }

  pub fn kill(&mut self) {
    if self.is_up() {
      if let ProcState::Some(inst) = &mut self.inst {
        let _result = inst.killer.kill();
      }
    }
  }

  #[cfg(not(windows))]
  pub fn stop(&mut self) {
    match self.stop_signal.clone() {
      StopSignal::SIGINT => self.send_signal(libc::SIGINT),
      StopSignal::SIGTERM => self.send_signal(libc::SIGTERM),
      StopSignal::SIGKILL => self.send_signal(libc::SIGKILL),
      StopSignal::SendKeys(keys) => {
        for key in keys {
          self.send_key(&key);
        }
      }
      StopSignal::HardKill => self.kill(),
    }
  }

  #[cfg(windows)]
  pub fn stop(&mut self) {
    match self.stop_signal.clone() {
      StopSignal::SIGINT => log::warn!("SIGINT signal is ignored on Windows"),
      StopSignal::SIGTERM => self.kill(),
      StopSignal::SIGKILL => self.kill(),
      StopSignal::SendKeys(keys) => {
        for key in keys {
          self.send_key(&key);
        }
      }
      StopSignal::HardKill => self.kill(),
    }
  }

  pub fn rename(&mut self, name: &str) {
    self.name.replace_range(.., &name);
    self.log_file = self
      .log_dir
      .as_ref()
      .map(|dir| crate::config::log_file_path(dir, &self.name));
  }

  #[cfg(not(windows))]
  fn send_signal(&mut self, sig: libc::c_int) {
    if let ProcState::Some(inst) = &self.inst {
      unsafe { libc::kill(inst.pid as i32, sig) };
    }
  }

  pub fn resize(&mut self, size: Rect) {
    let size = Size::new(size);
    if let ProcState::Some(inst) = &self.inst {
      inst.resize(&size);
    }
    self.size = size;
  }

  pub fn send_key(&mut self, key: &Key) {
    if self.is_up() {
      let application_cursor_keys = self
        .lock_vt()
        .map_or(false, |vt| vt.screen().application_cursor());
      let encoder = encode_key(
        key,
        KeyCodeEncodeModes {
          enable_csi_u_key_encoding: true,
          application_cursor_keys,
          newline_mode: false,
        },
      );
      match encoder {
        Ok(encoder) => {
          self.write_all(encoder.as_bytes());
        }
        Err(_) => {
          log::warn!("Failed to encode key: {}", key.to_string());
        }
      }
    }
  }

  pub fn write_all(&mut self, bytes: &[u8]) {
    if self.is_up() {
      if let Some(mut vt) = self.lock_vt_mut() {
        if vt.screen().scrollback() > 0 {
          vt.set_scrollback(0);
        }
      }
      if let ProcState::Some(inst) = &mut self.inst {
        inst.master.write_all(bytes).log_ignore();
      }
    }
  }

  pub fn scroll_up_lines(&mut self, n: usize) {
    match &mut self.copy_mode {
      CopyMode::None(_) => {
        if let Some(mut vt) = self.lock_vt_mut() {
          Self::scroll_vt_up(&mut vt, n);
        }
      }
      CopyMode::Start(screen, _) | CopyMode::Range(screen, _, _) => {
        Self::scroll_screen_up(screen, n)
      }
    }
  }

  fn scroll_vt_up(vt: &mut vt100::Parser, n: usize) {
    let pos = usize::saturating_add(vt.screen().scrollback(), n);
    vt.set_scrollback(pos);
  }

  fn scroll_screen_up(screen: &mut vt100::Screen, n: usize) {
    let pos = usize::saturating_add(screen.scrollback(), n);
    screen.set_scrollback(pos);
  }

  pub fn scroll_down_lines(&mut self, n: usize) {
    match &mut self.copy_mode {
      CopyMode::None(_) => {
        if let Some(mut vt) = self.lock_vt_mut() {
          Self::scroll_vt_down(&mut vt, n);
        }
      }
      CopyMode::Start(screen, _) | CopyMode::Range(screen, _, _) => {
        Self::scroll_screen_down(screen, n)
      }
    }
  }

  fn scroll_vt_down(vt: &mut vt100::Parser, n: usize) {
    let pos = usize::saturating_sub(vt.screen().scrollback(), n);
    vt.set_scrollback(pos);
  }

  fn scroll_screen_down(screen: &mut vt100::Screen, n: usize) {
    let pos = usize::saturating_sub(screen.scrollback(), n);
    screen.set_scrollback(pos);
  }

  pub fn scroll_half_screen_up(&mut self) {
    self.scroll_up_lines(self.size.height as usize / 2);
  }

  pub fn scroll_half_screen_down(&mut self) {
    self.scroll_down_lines(self.size.height as usize / 2);
  }
  
  pub fn clear_buffer(&mut self) {
    if let Some(mut vt) = self.lock_vt_mut() {
      // Get current parser dimensions and scrollback capacity
      let screen = vt.screen();
      let size = screen.size();
      let scrollback_len = screen.scrollback_len();

      // Reinitialize the parser to clear both visible area and scrollback
      *vt = vt100::Parser::new(size.0, size.1, scrollback_len);
    }
  }

  pub fn handle_mouse(
    &mut self,
    event: MouseEvent,
    term_area: Rect,
    config: &Config,
  ) {
    let copy_mode = match self.copy_mode {
      CopyMode::None(_) => false,
      CopyMode::Start(_, _) | CopyMode::Range(_, _, _) => true,
    };
    let mouse_mode = self
      .lock_vt()
      .map(|vt| vt.screen().mouse_protocol_mode())
      .unwrap_or_default();

    if copy_mode {
      match event.kind {
        MouseEventKind::Down(btn) => match btn {
          MouseButton::Left => {
            let scrollback = match &self.copy_mode {
              CopyMode::None(_) => unreachable!(),
              CopyMode::Start(screen, _) | CopyMode::Range(screen, _, _) => {
                screen.scrollback()
              }
            };
            self.copy_mode = CopyMode::None(Some(translate_mouse_pos(
              &event, &term_area, scrollback,
            )));
          }
          MouseButton::Right => {
            self.copy_mode = match std::mem::take(&mut self.copy_mode) {
              CopyMode::None(_) => unreachable!(),
              CopyMode::Start(screen, start)
              | CopyMode::Range(screen, start, _) => {
                let pos =
                  translate_mouse_pos(&event, &term_area, screen.scrollback());
                CopyMode::Range(screen, start, pos)
              }
            };
          }
          MouseButton::Middle => (),
        },
        MouseEventKind::Up(_) => (),
        MouseEventKind::Drag(MouseButton::Left) => {
          self.copy_mode = match std::mem::take(&mut self.copy_mode) {
            CopyMode::None(_) => unreachable!(),
            CopyMode::Start(screen, start)
            | CopyMode::Range(screen, start, _) => {
              let pos =
                translate_mouse_pos(&event, &term_area, screen.scrollback());
              CopyMode::Range(screen, start, pos)
            }
          };
        }
        MouseEventKind::Drag(_) => (),
        MouseEventKind::Moved => (),
        MouseEventKind::ScrollDown => match &mut self.copy_mode {
          CopyMode::None(_) => unreachable!(),
          CopyMode::Start(screen, _) | CopyMode::Range(screen, _, _) => {
            Self::scroll_screen_down(screen, config.mouse_scroll_speed);
          }
        },
        MouseEventKind::ScrollUp => match &mut self.copy_mode {
          CopyMode::None(_) => unreachable!(),
          CopyMode::Start(screen, _) | CopyMode::Range(screen, _, _) => {
            Self::scroll_screen_up(screen, config.mouse_scroll_speed);
          }
        },
      }
    } else {
      if let ProcState::Some(inst) = &mut self.inst {
        match mouse_mode {
          MouseProtocolMode::None => match event.kind {
            MouseEventKind::Down(btn) => match btn {
              MouseButton::Left => {
                if let Some(vt) = inst.vt.read().log_get() {
                  self.copy_mode = CopyMode::None(Some(translate_mouse_pos(
                    &event,
                    &term_area,
                    vt.screen().scrollback(),
                  )));
                }
              }
              MouseButton::Right | MouseButton::Middle => (),
            },
            MouseEventKind::Up(_) => (),
            MouseEventKind::Drag(MouseButton::Left) => {
              if let Some(vt) = inst.vt.read().log_get() {
                let pos = translate_mouse_pos(
                  &event,
                  &term_area,
                  vt.screen().scrollback(),
                );
                self.copy_mode = match std::mem::take(&mut self.copy_mode) {
                  CopyMode::None(pos_) => CopyMode::Range(
                    vt.screen().clone(),
                    pos_.unwrap_or_default(),
                    pos,
                  ),
                  CopyMode::Start(..) | CopyMode::Range(..) => {
                    unreachable!()
                  }
                };
              }
            }
            MouseEventKind::Drag(_) => (),
            MouseEventKind::Moved => (),
            MouseEventKind::ScrollDown => {
              if let Some(mut vt) = inst.vt.write().log_get() {
                Self::scroll_vt_down(&mut vt, config.mouse_scroll_speed);
              }
            }
            MouseEventKind::ScrollUp => {
              if let Some(mut vt) = inst.vt.write().log_get() {
                Self::scroll_vt_up(&mut vt, config.mouse_scroll_speed);
              }
            }
          },
          MouseProtocolMode::Press
          | MouseProtocolMode::PressRelease
          | MouseProtocolMode::ButtonMotion
          | MouseProtocolMode::AnyMotion => {
            let ev = MouseEvent {
              kind: event.kind,
              column: event.column - term_area.x,
              row: event.row - term_area.y,
              modifiers: event.modifiers,
            };
            let seq = encode_mouse_event(ev);
            let _r = inst.master.write_all(seq.as_bytes());
          }
        }
      }
    }
  }
}

fn translate_mouse_pos(
  event: &MouseEvent,
  area: &Rect,
  scrollback: usize,
) -> Pos {
  let x = (event.column - area.x) as i32;
  let y = (event.row - area.y) as i32 - scrollback as i32;
  Pos { y, x }
}

struct Size {
  width: u16,
  height: u16,
}

impl Size {
  fn new(rect: Rect) -> Size {
    Size {
      width: rect.width.max(3),
      height: rect.height.max(3),
    }
  }
}

pub enum CopyMode {
  None(Option<Pos>),
  Start(vt100::Screen, Pos),
  Range(vt100::Screen, Pos, Pos),
}

impl Default for CopyMode {
  fn default() -> Self {
    CopyMode::None(None)
  }
}

#[derive(
  Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize,
)]
pub struct Pos {
  pub y: i32,
  pub x: i32,
}

impl Pos {
  pub fn to_low_high<'a>(a: &'a Self, b: &'a Self) -> (&'a Self, &'a Self) {
    if a.y > b.y {
      return (b, a);
    } else if a.y == b.y && a.x > b.x {
      return (b, a);
    }
    (a, b)
  }

  pub fn within(start: &Self, end: &Self, target: &Self) -> bool {
    let y = target.y;
    let x = target.x;
    let (low, high) = Pos::to_low_high(start, end);

    if y > low.y {
      if y < high.y {
        true
      } else if y == high.y && x <= high.x {
        true
      } else {
        false
      }
    } else if y == low.y {
      if y < high.y {
        x >= low.x
      } else if y == high.y {
        x >= low.x && x <= high.x
      } else {
        false
      }
    } else {
      false
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{open_log_file, LogFileConfig};
  use std::io::Write;
  use std::time::SystemTime;

  fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let path = std::env::temp_dir()
      .join(format!("mprocs-{}-{}-{}", tag, std::process::id(), nanos));
    std::fs::create_dir_all(&path).unwrap();
    path
  }

  #[test]
  fn a_log_under_the_ceiling_is_appended_to() {
    let dir = temp_dir("log-append");
    let path = dir.join("api.log");
    std::fs::write(&path, b"from the previous run\n").unwrap();

    let cfg = LogFileConfig {
      path: path.clone(),
      max_bytes: 1024,
    };
    let mut file = open_log_file(&cfg, "api", 42, SystemTime::now()).unwrap();
    file.write_all(b"from this run\n").unwrap();
    drop(file);

    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("from the previous run"), "{}", body);
    assert!(body.contains("=== mprocs: api started, pid=42, at="), "{}", body);
    assert!(body.contains("from this run"), "{}", body);

    std::fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn a_log_over_the_ceiling_is_truncated_on_start() {
    let dir = temp_dir("log-truncate");
    let path = dir.join("api.log");
    std::fs::write(&path, vec![b'x'; 4096]).unwrap();

    let cfg = LogFileConfig {
      path: path.clone(),
      max_bytes: 1024,
    };
    let file = open_log_file(&cfg, "api", 42, SystemTime::now()).unwrap();
    drop(file);

    let body = std::fs::read_to_string(&path).unwrap();
    assert!(!body.contains('x'), "the old content survived: {} bytes", body.len());
    assert!(body.contains("=== mprocs: api started"), "{}", body);

    std::fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn an_unopenable_log_does_not_take_the_process_down() {
    let dir = temp_dir("log-unopenable");
    // A directory where the log file should be: opening it must fail.
    let path = dir.join("api.log");
    std::fs::create_dir(&path).unwrap();

    let cfg = LogFileConfig {
      path: path.clone(),
      max_bytes: 1024,
    };
    assert!(open_log_file(&cfg, "api", 42, SystemTime::now()).is_none());

    std::fs::remove_dir_all(&dir).unwrap();
  }
}
