use anyhow::bail;
use crossterm::event::{Event, KeyCode, KeyEvent, MouseButton, MouseEventKind};
use futures::{future::FutureExt, select};
use tokio::{
  io::AsyncReadExt,
  sync::mpsc::{Receiver, UnboundedReceiver, UnboundedSender},
};
use tui::{
  layout::{Constraint, Direction, Layout, Margin, Rect},
  Terminal,
};
use tui_input::Input;

use crate::{
  clipboard::copy,
  config::{CmdConfig, Config, ProcConfig, ServerConfig},
  ctl_server::{
    CtlMessage, CtlRequest, CtlResponse, ERR_NO_MATCH,
  },
  event::{AppEvent, CopyMove},
  key::Key,
  keymap::Keymap,
  proc::{CopyMode, Pos, Proc, ProcState, ProcUpdate, StopSignal},
  protocol::{CltToSrv, ProxyBackend, SrvToClt},
  state::{Modal, Scope, State},
  ui_add_proc::render_input_dialog,
  ui_confirm_quit::render_confirm_quit,
  ui_keymap::render_keymap,
  ui_procs::{procs_check_hit, procs_get_clicked_index, render_procs},
  ui_remove_proc::render_remove_proc,
  ui_term::{render_term, term_check_hit},
  ui_zoom_tip::render_zoom_tip,
};

type Term = Terminal<ProxyBackend>;

enum LoopAction {
  Render,
  Skip,
  ForceQuit,
}

pub struct App {
  config: Config,
  keymap: Keymap,
  terminal: Term,
  state: State,
  client_rx: Receiver<CltToSrv>,
  client_tx: UnboundedSender<SrvToClt>,
  upd_rx: UnboundedReceiver<(usize, ProcUpdate)>,
  upd_tx: UnboundedSender<(usize, ProcUpdate)>,
  ev_rx: UnboundedReceiver<AppEvent>,
  ev_tx: UnboundedSender<AppEvent>,
  /// Requests coming from the control socket. Stays inert when no socket is
  /// configured: nobody ever writes on the other end.
  ctl_rx: UnboundedReceiver<CtlMessage>,
}

/// Matches a process name against a control pattern: an exact name, or a
/// single `*` used as a prefix, a suffix, or "everything".
fn matches(pattern: &str, name: &str) -> bool {
  match pattern.split_once('*') {
    None => pattern == name,
    Some((prefix, suffix)) => {
      name.len() >= prefix.len() + suffix.len()
        && name.starts_with(prefix)
        && name.ends_with(suffix)
    }
  }
}

impl App {
  pub async fn run(self) -> anyhow::Result<()> {
    let (exit_trigger, exit_listener) = triggered::trigger();

    let server_thread = if let Some(ref server_addr) = self.config.server {
      let server = match server_addr {
        ServerConfig::Tcp(addr) => tokio::net::TcpListener::bind(addr).await?,
      };

      let ev_tx = self.ev_tx.clone();
      let server_thread = tokio::spawn(async move {
        loop {
          let on_exit = exit_listener.clone();
          let mut socket: tokio::net::TcpStream = select! {
            _ = on_exit.fuse() => break,
            client = server.accept().fuse() => {
              if let Ok((socket, _)) = client {
                socket
              } else {
                break;
              }
            }
          };

          let ctl_tx = ev_tx.clone();
          let on_exit = exit_listener.clone();
          tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::with_capacity(32);
            let () = select! {
              _ = on_exit.fuse() => return,
              count = socket.read_to_end(&mut buf).fuse() => {
                if count.is_err() {
                  return;
                }
              }
            };
            let msg: AppEvent = serde_yaml::from_slice(buf.as_slice()).unwrap();
            // log::info!("Received remote command: {:?}", msg);
            ctl_tx.send(msg).unwrap();
          });
        }
      });
      Some(server_thread)
    } else {
      None
    };

    let result = self.main_loop().await;

    exit_trigger.trigger();
    if let Some(server_thread) = server_thread {
      let _ = server_thread.await;
    }

    result
  }

  async fn main_loop(mut self) -> anyhow::Result<()> {
    let mut last_term_size = {
      let area = self.get_layout().term_area();
      self.start_procs(area)?;
      (area.width, area.height)
    };

    let mut render_needed = true;
    loop {
      if render_needed {
        self.terminal.draw(|f| {
          let layout = AppLayout::new(
            f.size(),
            self.state.scope.is_zoomed(),
            &self.config,
          );

          {
            let term_area = layout.term_area();
            let term_size = (term_area.width, term_area.height);
            if last_term_size != term_size {
              last_term_size = term_size;
              for proc in &mut self.state.procs {
                proc.resize(term_area);
              }
            }
          }

          render_procs(layout.procs, f, &mut self.state);
          render_term(layout.term, f, &mut self.state);
          render_keymap(layout.keymap, f, &mut self.state, &self.keymap);
          render_zoom_tip(layout.zoom_banner, f, &self.keymap);

          if let Some(modal) = &mut self.state.modal {
            match modal {
              Modal::AddProc { input } => {
                render_input_dialog(f.size(), "Add process", f, input);
              }
              Modal::RenameProc { input } => {
                render_input_dialog(f.size(), "Rename process", f, input);
              }
              Modal::RemoveProc { id: _ } => {
                render_remove_proc(f.size(), f);
              }
              Modal::Quit => {
                render_confirm_quit(f.size(), f);
              }
            }
          }
        })?;
      }

      let loop_action = select! {
        event = self.client_rx.recv().fuse() => {
          if let Some(CltToSrv::Key(event)) = event {
            self.handle_input(Some(Ok(event)))
          } else {
            LoopAction::Skip
          }
        }
        event = self.upd_rx.recv().fuse() => {
          if let Some(event) = event {
            self.handle_proc_update(event)
          } else {
            LoopAction::Skip
          }
        }
        event = self.ev_rx.recv().fuse() => {
          if let Some(event) = event {
            self.handle_event(&event)
          } else {
            LoopAction::Skip
          }
        }
        msg = self.ctl_rx.recv().fuse() => {
          if let Some((req, reply)) = msg {
            let quitting = matches!(req, CtlRequest::Shutdown {});
            let (response, action) = self.handle_ctl(req);
            // An error here only means the client hung up before the answer.
            let _ = reply.send(response);
            if quitting {
              // Answer first, shut down after: otherwise the client never
              // gets the response.
              self.handle_event(&AppEvent::Quit)
            } else {
              action
            }
          } else {
            LoopAction::Skip
          }
        }
      };

      if self.state.quitting && self.state.all_procs_down() {
        break;
      }

      match loop_action {
        LoopAction::Render => {
          render_needed = true;
        }
        LoopAction::Skip => {
          render_needed = false;
        }
        LoopAction::ForceQuit => break,
      };
    }

    Ok(())
  }

  fn start_procs(&mut self, size: Rect) -> anyhow::Result<()> {
    let log_dir = self.config.log_dir.clone();
    let log_max_bytes = self.config.log_max_bytes;
    let mut procs = self
      .config
      .procs
      .iter()
      .map(|proc_cfg| {
        Proc::new(
          proc_cfg.name.clone(),
          proc_cfg,
          self.upd_tx.clone(),
          size,
          log_dir.as_deref(),
          log_max_bytes,
        )
      })
      .collect::<Vec<_>>();

    self.state.procs.append(&mut procs);

    Ok(())
  }

  fn handle_input(
    &mut self,
    event: Option<crossterm::Result<Event>>,
  ) -> LoopAction {
    let event = match event {
      Some(Ok(event)) => event,
      Some(Err(err)) => {
        log::warn!("Crossterm input error: {}", err.to_string());
        return LoopAction::Skip;
      }
      None => {
        log::warn!("Crossterm input is None.");
        return LoopAction::Skip;
      }
    };

    {
      let mut ret: Option<LoopAction> = None;
      let mut reset_modal = false;
      if let Some(modal) = &mut self.state.modal {
        match modal {
          Modal::AddProc { input } => {
            match event {
              Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
              }) if modifiers.is_empty() => {
                reset_modal = true;
                self
                  .ev_tx
                  .send(AppEvent::AddProc {
                    cmd: input.value().to_string(),
                  })
                  .unwrap();
                // Skip because AddProc event will immediately rerender.
                ret = Some(LoopAction::Skip);
              }
              Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers,
              }) if modifiers.is_empty() => {
                reset_modal = true;
                ret = Some(LoopAction::Render);
              }
              _ => (),
            }

            let req = tui_input::backend::crossterm::to_input_request(event);
            if let Some(req) = req {
              input.handle(req);
              ret = Some(LoopAction::Render);
            }
          }
          Modal::RenameProc { input } => {
            match event {
              Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
              }) if modifiers.is_empty() => {
                reset_modal = true;
                self
                  .ev_tx
                  .send(AppEvent::RenameProc {
                    name: input.value().to_string(),
                  })
                  .unwrap();
                // Skip because RenameProc event will immediately rerender.
                ret = Some(LoopAction::Skip);
              }
              Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers,
              }) if modifiers.is_empty() => {
                reset_modal = true;
                ret = Some(LoopAction::Render);
              }
              _ => (),
            }

            let req = tui_input::backend::crossterm::to_input_request(event);
            if let Some(req) = req {
              input.handle(req);
              ret = Some(LoopAction::Render);
            }
          }
          Modal::RemoveProc { id } => {
            match event {
              Event::Key(KeyEvent {
                code: KeyCode::Char('y'),
                modifiers,
              }) if modifiers.is_empty() => {
                reset_modal = true;
                self.ev_tx.send(AppEvent::RemoveProc { id: *id }).unwrap();
                // Skip because RemoveProc event will immediately rerender.
                ret = Some(LoopAction::Skip);
              }
              Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers,
              })
              | Event::Key(KeyEvent {
                code: KeyCode::Char('n'),
                modifiers,
              }) if modifiers.is_empty() => {
                reset_modal = true;
                ret = Some(LoopAction::Render);
              }
              _ => (),
            }
          }
          Modal::Quit => match event {
            Event::Key(KeyEvent {
              code: KeyCode::Char('y'),
              modifiers,
            }) if modifiers.is_empty() => {
              reset_modal = true;
              self.ev_tx.send(AppEvent::Quit).unwrap();
              ret = Some(LoopAction::Skip);
            }
            Event::Key(KeyEvent {
              code: KeyCode::Esc,
              modifiers,
            })
            | Event::Key(KeyEvent {
              code: KeyCode::Char('n'),
              modifiers,
            }) if modifiers.is_empty() => {
              reset_modal = true;
              ret = Some(LoopAction::Render);
            }
            _ => (),
          },
        };
      }

      if reset_modal {
        self.state.modal = None;
      }
      if let Some(ret) = ret {
        return ret;
      }
    }

    match event {
      Event::Key(key) => {
        let key = Key::from(key);
        let group = self.state.get_keymap_group();
        if let Some(bound) = self.keymap.resolve(group, &key) {
          let bound = bound.clone();
          self.handle_event(&bound)
        } else {
          match self.state.scope {
            Scope::Procs => LoopAction::Skip,
            Scope::Term | Scope::TermZoom => {
              self.handle_event(&AppEvent::SendKey { key })
            }
          }
        }
      }
      Event::Mouse(mev) => {
        if mev.kind == MouseEventKind::Moved {
          return LoopAction::Skip;
        }

        let layout = self.get_layout();
        if term_check_hit(layout.term_area(), mev.column, mev.row) {
          match (self.state.scope, mev.kind) {
            (Scope::Procs, MouseEventKind::Down(_)) => {
              self.state.scope = Scope::Term
            }
            _ => (),
          }
          if let Some(proc) = self.state.get_current_proc_mut() {
            proc.handle_mouse(mev, layout.term_area(), &self.config);
          }
        } else if procs_check_hit(layout.procs, mev.column, mev.row) {
          match (self.state.scope, mev.kind) {
            (Scope::Term, MouseEventKind::Down(_)) => {
              self.state.scope = Scope::Procs
            }
            _ => (),
          }
          match mev.kind {
            MouseEventKind::Down(btn) => match btn {
              MouseButton::Left => {
                if let Some(index) = procs_get_clicked_index(
                  layout.procs,
                  mev.column,
                  mev.row,
                  &self.state,
                ) {
                  self.state.select_proc(index);
                }
              }
              MouseButton::Right | MouseButton::Middle => (),
            },
            MouseEventKind::Up(_) => (),
            MouseEventKind::Drag(_) => (),
            MouseEventKind::Moved => (),
            MouseEventKind::ScrollDown => {
              if self.state.selected < self.state.procs.len().saturating_sub(1)
              {
                let index = self.state.selected + 1;
                self.state.select_proc(index);
              }
            }
            MouseEventKind::ScrollUp => {
              if self.state.selected > 0 {
                let index = self.state.selected - 1;
                self.state.select_proc(index);
              }
            }
          }
        }
        LoopAction::Render
      }
      Event::Resize(width, height) => {
        let (width, height) = if cfg!(windows) {
          crossterm::terminal::size().unwrap()
        } else {
          (width, height)
        };


        let area = AppLayout::new(
          Rect::new(0, 0, width, height),
          self.state.scope.is_zoomed(),
          &self.config,
        )
        .term_area();

        self.terminal.backend_mut().set_size(width, height);
        self.terminal.resize(area);

        for proc in &mut self.state.procs {
          proc.resize(area);
        }

        LoopAction::Render
      }
    }
  }

  fn handle_event(&mut self, event: &AppEvent) -> LoopAction {
    match event {
      AppEvent::Batch { cmds } => {
        let mut ret = LoopAction::Skip;
        for cmd in cmds {
          match self.handle_event(cmd) {
            LoopAction::Render => ret = LoopAction::Render,
            LoopAction::Skip => (),
            LoopAction::ForceQuit => return LoopAction::ForceQuit,
          };
        }
        ret
      }

      AppEvent::QuitOrAsk => {
        let have_running = self.state.procs.iter().any(|p| p.is_up());
        if have_running {
          self.state.modal = Some(Modal::Quit);
        } else {
          self.state.quitting = true;
        }
        LoopAction::Render
      }
      AppEvent::Quit => {
        self.state.quitting = true;
        for proc in self.state.procs.iter_mut() {
          if proc.is_up() {
            proc.stop();
          }
        }
        LoopAction::Render
      }
      AppEvent::ForceQuit => {
        for proc in self.state.procs.iter_mut() {
          if proc.is_up() {
            proc.kill();
          }
        }
        LoopAction::ForceQuit
      }

      AppEvent::ToggleFocus => {
        self.state.scope = self.state.scope.toggle();
        LoopAction::Render
      }
      AppEvent::FocusProcs => {
        self.state.scope = Scope::Procs;
        LoopAction::Render
      }
      AppEvent::FocusTerm => {
        self.state.scope = Scope::Term;
        LoopAction::Render
      }
      AppEvent::Zoom => {
        self.state.scope = Scope::TermZoom;
        LoopAction::Render
      }

      AppEvent::NextProc => {
        let mut next = self.state.selected + 1;
        if next >= self.state.procs.len() {
          next = 0;
        }
        self.state.select_proc(next);
        LoopAction::Render
      }
      AppEvent::PrevProc => {
        let next = if self.state.selected > 0 {
          self.state.selected - 1
        } else {
          self.state.procs.len() - 1
        };
        self.state.select_proc(next);
        LoopAction::Render
      }
      AppEvent::SelectProc { index } => {
        self.state.select_proc(*index);
        LoopAction::Render
      }

      AppEvent::StartProc => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.start();
        }
        LoopAction::Skip
      }
      AppEvent::TermProc => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.stop();
        }
        LoopAction::Skip
      }
      AppEvent::KillProc => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.kill();
        }
        LoopAction::Skip
      }
      AppEvent::RestartProc => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          if proc.is_up() {
            proc.stop();
            proc.to_restart = true;
          } else {
            proc.start();
          }
        }
        LoopAction::Skip
      }
      AppEvent::ForceRestartProc => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          if proc.is_up() {
            proc.kill();
            proc.to_restart = true;
          } else {
            proc.start();
          }
        }
        LoopAction::Skip
      }

      AppEvent::ScrollUpLines { n } => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.scroll_up_lines(*n);
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
      AppEvent::ScrollDownLines { n } => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.scroll_down_lines(*n);
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
      AppEvent::ScrollUp => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.scroll_half_screen_up();
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
      AppEvent::ScrollDown => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.scroll_half_screen_down();
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
      AppEvent::ShowAddProc => {
        self.state.modal = Some(Modal::AddProc {
          input: Input::default(),
        });
        LoopAction::Render
      }
      AppEvent::AddProc { cmd } => {
        let log_dir = self.config.log_dir.clone();
        let log_max_bytes = self.config.log_max_bytes;
        let proc = Proc::new(
          cmd.to_string(),
          &ProcConfig {
            name: cmd.to_string(),
            cmd: CmdConfig::Shell {
              shell: cmd.to_string(),
            },
            cwd: None,
            env: None,
            autostart: true,
            stop: StopSignal::default(),
          },
          self.upd_tx.clone(),
          self.get_layout().term_area(),
          log_dir.as_deref(),
          log_max_bytes,
        );
        self.state.procs.push(proc);
        LoopAction::Render
      }
      AppEvent::ShowRemoveProc => {
        let id = self
          .state
          .get_current_proc()
          .map(|proc| if proc.is_up() { None } else { Some(proc.id) })
          .flatten();
        match id {
          Some(id) => {
            self.state.modal = Some(Modal::RemoveProc { id });
            LoopAction::Render
          }
          None => LoopAction::Skip,
        }
      }
      AppEvent::RemoveProc { id } => {
        self
          .state
          .procs
          .retain(|proc| proc.is_up() || proc.id != *id);
        LoopAction::Render
      }

      AppEvent::ShowRenameProc => {
        self.state.modal = Some(Modal::RenameProc {
          input: Input::default(),
        });
        LoopAction::Render
      }
      AppEvent::RenameProc { name } => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.rename(name);
          LoopAction::Render
        } else {
          LoopAction::Skip
        }
      }

      AppEvent::CopyModeEnter => {
        let switched = match self.state.get_current_proc_mut() {
          Some(proc) => match &mut proc.inst {
            ProcState::None => false,
            ProcState::Some(inst) => {
              let screen = inst.vt.read().unwrap().screen().clone();
              let y = (screen.size().0 - 1) as i32;
              proc.copy_mode = CopyMode::Start(screen, Pos { y, x: 0 });
              true
            }
            ProcState::Error(_) => false,
          },
          None => false,
        };
        if switched {
          self.state.scope = Scope::Term;
        }
        LoopAction::Render
      }
      AppEvent::CopyModeLeave => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.copy_mode = CopyMode::None(None);
        }
        LoopAction::Render
      }
      AppEvent::CopyModeMove { dir } => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          match &proc.inst {
            ProcState::None => (),
            ProcState::Some(inst) => {
              let vt = inst.vt.read().unwrap();
              let screen = vt.screen();
              match &mut proc.copy_mode {
                CopyMode::None(_) => (),
                CopyMode::Start(_, pos_) | CopyMode::Range(_, _, pos_) => {
                  match dir {
                    CopyMove::Up => {
                      if pos_.y > -(screen.scrollback_len() as i32) {
                        pos_.y -= 1
                      }
                    }
                    CopyMove::Right => {
                      if pos_.x + 1 < screen.size().1 as i32 {
                        pos_.x += 1
                      }
                    }
                    CopyMove::Left => {
                      if pos_.x > 0 {
                        pos_.x -= 1
                      }
                    }
                    CopyMove::Down => {
                      if pos_.y + 1 < screen.size().0 as i32 {
                        pos_.y += 1
                      }
                    }
                  };
                }
              }
            }
            ProcState::Error(_) => (),
          }
        }
        LoopAction::Render
      }
      AppEvent::CopyModeEnd => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.copy_mode = match std::mem::take(&mut proc.copy_mode) {
            CopyMode::Start(screen, start) => {
              CopyMode::Range(screen, start.clone(), start)
            }
            other => other,
          };
        }
        LoopAction::Render
      }
      AppEvent::CopyModeCopy => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          if let CopyMode::Range(screen, start, end) = &proc.copy_mode {
            let (low, high) = Pos::to_low_high(start, end);
            let text = screen.get_selected_text(low.x, low.y, high.x, high.y);

            copy(text.as_str());
          }
          proc.copy_mode = CopyMode::None(None);
        }
        LoopAction::Render
      }

      AppEvent::SendKey { key } => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.send_key(key);
        }
        LoopAction::Skip
      }

      AppEvent::ClearBuffer => {
        if let Some(proc) = self.state.get_current_proc_mut() {
          proc.clear_buffer();
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
    }
  }

  /// Answers a control request. Runs inside the main loop, so it sees the
  /// same `State` the TUI is drawing, with no lock and no race.
  ///
  /// `Shutdown` is answered here but performed by the caller, after the
  /// response has been handed back to the client.
  fn handle_ctl(&mut self, req: CtlRequest) -> (CtlResponse, LoopAction) {
    match req {
      CtlRequest::Ls { pattern } => {
        let procs = self
          .state
          .procs
          .iter()
          .filter(|proc| {
            pattern
              .as_deref()
              .map_or(true, |pattern| matches(pattern, &proc.name))
          })
          .map(describe_proc)
          .collect::<Vec<_>>();
        (
          CtlResponse::Ok(serde_json::json!({ "procs": procs })),
          LoopAction::Skip,
        )
      }

      CtlRequest::Screen { name } => {
        let response = match self
          .state
          .procs
          .iter()
          .find(|proc| proc.name == name)
        {
          None => CtlResponse::err(
            ERR_NO_MATCH,
            format!("no proc named '{}'", name),
          ),
          // `lock_vt` and not `lock_vt_mut`: the mutable one is for whoever
          // moves the scrollback offset, which is what the user is looking at.
          Some(proc) => match proc.lock_vt() {
            None => {
              CtlResponse::Ok(serde_json::json!({ "screen": serde_json::Value::Null }))
            }
            Some(vt) => CtlResponse::Ok(
              serde_json::json!({ "screen": vt.screen().contents() }),
            ),
          },
        };
        (response, LoopAction::Skip)
      }

      // These act on every process matching the pattern, not on the one
      // selected in the TUI, so they cannot go through `AppEvent`.
      CtlRequest::Start { pattern } => {
        let matched = self.for_each_matching(&pattern, |proc| proc.start());
        (matched_response(matched), LoopAction::Render)
      }
      CtlRequest::Stop { pattern } => {
        let matched = self.for_each_matching(&pattern, |proc| proc.stop());
        (matched_response(matched), LoopAction::Render)
      }
      CtlRequest::Kill { pattern } => {
        let matched = self.for_each_matching(&pattern, |proc| proc.kill());
        (matched_response(matched), LoopAction::Render)
      }
      CtlRequest::Restart { pattern } => {
        // The actual restart is done by `handle_proc_update` when
        // `ProcUpdate::Stopped` arrives.
        let matched = self.for_each_matching(&pattern, |proc| {
          if proc.is_up() {
            proc.stop();
            proc.to_restart = true;
          } else {
            proc.start();
          }
        });
        (matched_response(matched), LoopAction::Render)
      }

      CtlRequest::Shutdown {} => {
        (CtlResponse::Ok(serde_json::json!({})), LoopAction::Render)
      }
    }
  }

  /// Runs `f` on every process whose name matches `pattern`. Returns how many
  /// were touched; zero is not an error.
  fn for_each_matching<F: FnMut(&mut Proc)>(
    &mut self,
    pattern: &str,
    mut f: F,
  ) -> usize {
    let mut matched = 0;
    for proc in self.state.procs.iter_mut() {
      if matches(pattern, &proc.name) {
        f(proc);
        matched += 1;
      }
    }
    matched
  }

  fn handle_proc_update(&mut self, event: (usize, ProcUpdate)) -> LoopAction {
    match event.1 {
      ProcUpdate::Render => {
        let cur_proc_id =
          self.state.get_current_proc().map_or(usize::MAX, |p| p.id);
        if let Some(proc) = self.state.get_proc_mut(event.0) {
          if proc.id != cur_proc_id {
            proc.changed = true;
          }
          return LoopAction::Render;
        }
        LoopAction::Skip
      }
      ProcUpdate::Stopped { exit_code, signal } => {
        if let Some(proc) = self.state.get_proc_mut(event.0) {
          proc.last_exit_code = exit_code;
          proc.last_signal = signal;
          if proc.to_restart {
            // `start()` clears the exit status again: a new run begins.
            proc.start();
            proc.to_restart = false;
          }
        }
        LoopAction::Render
      }
      ProcUpdate::Started => LoopAction::Render,
    }
  }

  fn get_layout(&mut self) -> AppLayout {
    AppLayout::new(
      self.terminal.get_frame().size(),
      self.state.scope.is_zoomed(),
      &self.config,
    )
  }
}

fn matched_response(matched: usize) -> CtlResponse {
  CtlResponse::Ok(serde_json::json!({ "matched": matched }))
}

/// One entry of the `ls` answer. See `docs/ctl-rpc/01-protocol.md#ls`.
fn describe_proc(proc: &Proc) -> serde_json::Value {
  use serde_json::{Map, Value};

  let mut obj = Map::new();
  obj.insert("name".to_string(), Value::from(proc.name.as_str()));
  obj.insert("id".to_string(), Value::from(proc.id));

  match &proc.inst {
    // `is_up()` alone cannot tell `idle` from `exited`: both are down.
    ProcState::None => {
      obj.insert("state".to_string(), Value::from("idle"));
    }
    ProcState::Some(inst) => {
      let running = inst.running.load(std::sync::atomic::Ordering::Relaxed);
      if running {
        obj.insert("state".to_string(), Value::from("running"));
        // Only while running: the pid of a dead process is a trap.
        obj.insert("pid".to_string(), Value::from(inst.pid));
      } else {
        obj.insert("state".to_string(), Value::from("exited"));
        obj.insert(
          "exit_code".to_string(),
          proc.last_exit_code.map_or(Value::Null, Value::from),
        );
        obj.insert(
          "signal".to_string(),
          proc
            .last_signal
            .as_deref()
            .map_or(Value::Null, Value::from),
        );
      }
      if let Ok(since_epoch) =
        inst.started_at.duration_since(std::time::UNIX_EPOCH)
      {
        obj.insert(
          "started_at".to_string(),
          Value::from(since_epoch.as_secs()),
        );
      }
    }
    ProcState::Error(message) => {
      obj.insert("state".to_string(), Value::from("error"));
      obj.insert("message".to_string(), Value::from(message.as_str()));
    }
  }

  obj.insert(
    "log_file".to_string(),
    proc
      .log_file
      .as_ref()
      .map_or(Value::Null, |path| Value::from(path.to_string_lossy())),
  );

  Value::Object(obj)
}

struct AppLayout {
  procs: Rect,
  term: Rect,
  keymap: Rect,
  zoom_banner: Rect,
}

impl AppLayout {
  pub fn new(area: Rect, zoom: bool, config: &Config) -> Self {
    let keymap_h = if zoom || config.hide_keymap_window {
      0
    } else {
      3
    };
    let procs_w = if zoom {
      0
    } else {
      config.proc_list_width as u16
    };
    let zoom_banner_h = if zoom { 1 } else { 0 };
    let top_bot = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Min(1), Constraint::Length(keymap_h)])
      .split(area);
    let chunks = Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Length(procs_w), Constraint::Min(2)].as_ref())
      .split(top_bot[0]);
    let term_zoom = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Length(zoom_banner_h), Constraint::Min(1)])
      .split(chunks[1]);

    Self {
      procs: chunks[0],
      term: term_zoom[1],
      keymap: top_bot[1],
      zoom_banner: term_zoom[0],
    }
  }

  pub fn term_area(&self) -> Rect {
    self.term.inner(&Margin {
      vertical: 1,
      horizontal: 1,
    })
  }
}

pub async fn server_main(
  config: Config,
  keymap: Keymap,
  client_tx: tokio::sync::mpsc::UnboundedSender<SrvToClt>,
  mut client_rx: tokio::sync::mpsc::Receiver<CltToSrv>,
  ctl_rx: UnboundedReceiver<CtlMessage>,
) -> anyhow::Result<()> {
  let init = client_rx
    .recv()
    .await
    .ok_or_else(|| anyhow::Error::msg("Expected init message."))?;
  let backend = match init {
    CltToSrv::Init { width, height } => {
      let proxy_backend = ProxyBackend {
        tx: client_tx.clone(),
        width,
        height,
      };
      proxy_backend
    }
    _ => bail!("Expected init message."),
  };

  let terminal = Terminal::new(backend)?;

  let (upd_tx, upd_rx) =
    tokio::sync::mpsc::unbounded_channel::<(usize, ProcUpdate)>();
  let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();

  let state = State {
    scope: Scope::Procs,
    procs: Vec::new(),
    selected: 0,

    modal: None,

    quitting: false,
  };

  let app = App {
    config,
    keymap,
    terminal,
    state,
    client_rx,
    client_tx,
    upd_rx,
    upd_tx,

    ev_rx,
    ev_tx,
    ctl_rx,
  };
  let client_tx = app.client_tx.clone();
  app.run().await?;
  client_tx.send(SrvToClt::Quit).unwrap();

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::matches;

  #[test]
  fn exact_name() {
    assert!(matches("api", "api"));
    assert!(!matches("api", "api2"));
    assert!(!matches("api", "2api"));
    assert!(!matches("api", "API"));
  }

  #[test]
  fn everything() {
    assert!(matches("*", "api"));
    assert!(matches("*", ""));
    assert!(matches("*", "sidekiq-worker"));
  }

  #[test]
  fn prefix() {
    assert!(matches("web*", "web"));
    assert!(matches("web*", "webpack"));
    assert!(!matches("web*", "api"));
    assert!(!matches("web*", "we"));
  }

  #[test]
  fn suffix() {
    assert!(matches("*worker", "worker"));
    assert!(matches("*worker", "sidekiq-worker"));
    assert!(!matches("*worker", "worker-x"));
    assert!(!matches("*worker", "api"));
  }

  #[test]
  fn prefix_and_suffix_do_not_overlap() {
    // "ab" must not satisfy both sides of "ab*ab" with the same characters.
    assert!(!matches("ab*ab", "ab"));
    assert!(matches("ab*ab", "abab"));
    assert!(matches("ab*ab", "abXab"));
  }
}
