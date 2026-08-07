use std::{
    ffi::OsString,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use super::{
    modules::{
        god_mode_ui::godui_ws_session_recent_frames,
        websocket_routes::{ws_validate_target, WsConfig},
    },
    servecore_ws_send, servecore_ws_send_text_frames, ServecoreEngine, ServecoreSharedState,
    ServecoreWsKind,
};
use axum::extract::ws::{Message, WebSocket};
use maw_routing::{
    resolve_target as resolve_route_target, MawConfig as RouteConfig, ResolveResult as RouteResult,
    Session as RouteSession, Window as RouteWindow,
};
use maw_tmux::TmuxSession;
use portable_pty::{CommandBuilder, PtySize};

const SERVEENGINE_CHILD_TIMEOUT_SECS: u64 = 30;
const SERVEENGINE_CHILD_TIMEOUT_ENV: &str = "MAW_RS_SERVE_CHILD_TIMEOUT_SECS";
const SERVEENGINE_FAKE_TMUX_LOG_ENV: &str = "MAW_RS_SERVECORE_FAKE_TMUX_LOG";
const SERVEENGINE_FAKE_CAPTURE_ENV: &str = "MAW_RS_SERVECORE_FAKE_CAPTURE";
const SERVEENGINE_PTY_PROGRAM_ENV: &str = "MAW_RS_SERVECORE_PTY_PROGRAM";
const SERVEENGINE_PTY_TERM: &str = "screen-256color";

#[derive(Debug)]
pub struct ServecoreNativeEngine;

impl ServecoreEngine for ServecoreNativeEngine {
    fn servecore_engine_name(&self) -> &'static str {
        "maw-rs"
    }

    fn servecore_ws_text(
        &self,
        kind: ServecoreWsKind,
        text: &str,
        target: Option<&str>,
    ) -> Option<String> {
        if !matches!(kind, ServecoreWsKind::Engine) {
            return Some(text.to_owned());
        }
        Some(serveengine_ws_text(text, target))
    }
}

pub trait ServecoreExecRunner: Send + Sync {
    /// Runs a controlled maw child process for serve orchestration.
    ///
    /// # Errors
    ///
    /// Returns an error when the runner cannot spawn, wait for, or complete the
    /// child process within its bounded timeout.
    fn servecore_run(&self, argv: &[String], cwd: &Path) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct ServecoreProcessRunner;

impl ServecoreExecRunner for ServecoreProcessRunner {
    fn servecore_run(&self, argv: &[String], cwd: &Path) -> Result<(), String> {
        serveengine_run_with_timeout(
            &serveengine_self_bin()?,
            argv,
            cwd,
            serveengine_child_timeout(),
        )
    }
}

fn serveengine_child_timeout() -> Duration {
    std::env::var(SERVEENGINE_CHILD_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or_else(
            || Duration::from_secs(SERVEENGINE_CHILD_TIMEOUT_SECS),
            Duration::from_secs,
        )
}

pub(crate) fn serveengine_self_bin() -> Result<PathBuf, String> {
    std::env::var_os("MAW_RS_SELF_BIN")
        .map(PathBuf::from)
        .map_or_else(
            || {
                std::env::current_exe()
                    .map_err(|error| format!("serve-orchestration: current_exe failed: {error}"))
            },
            Ok,
        )
}

pub(crate) fn serveengine_run_with_timeout(
    program: &Path,
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir(cwd)
        .spawn()
        .map_err(|error| format!("serve-orchestration: spawn failed: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("serve-orchestration: wait failed: {error}"))?
        {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("serve-orchestration: workon exited with {status}"))
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("serve-orchestration: workon timed out".to_owned());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub(crate) fn serveengine_tmux_capture(target: &str) -> Result<String, String> {
    serveengine_tmux_capture_lines(target, None)
}

pub(crate) fn serveengine_tmux_capture_lines(
    target: &str,
    lines: Option<u32>,
) -> Result<String, String> {
    let target = serveengine_ws_validate_target(target)?;
    if let Ok(capture) = std::env::var(SERVEENGINE_FAKE_CAPTURE_ENV) {
        return Ok(serveengine_tail_capture_lines(capture, lines));
    }
    let mut tmux = maw_tmux::TmuxClient::local();
    let sessions = tmux.list_all();
    let resolved = serveengine_resolve_capture_target(&target, &sessions);
    let capture_lines = lines.filter(|line_count| *line_count > 50);
    tmux.capture(&resolved, capture_lines)
        .map(|capture| serveengine_tail_capture_lines(capture, lines))
        .map_err(|error| format!("serve-ws: capture failed: {}", error.message))
}

fn serveengine_tail_capture_lines(capture: String, lines: Option<u32>) -> String {
    let Some(lines) = lines.filter(|line_count| *line_count <= 50) else {
        return capture;
    };
    let lines = usize::try_from(lines).unwrap_or(usize::MAX);
    let chunks = capture.split_inclusive('\n').collect::<Vec<_>>();
    if chunks.len() <= lines {
        return capture;
    }
    chunks[chunks.len() - lines..].concat()
}

fn serveengine_resolve_capture_target(target: &str, sessions: &[TmuxSession]) -> String {
    if target.trim().is_empty() || target.starts_with('%') {
        return target.to_owned();
    }
    let route_sessions = sessions
        .iter()
        .map(|session| RouteSession {
            name: session.name.clone(),
            source: None,
            windows: session
                .windows
                .iter()
                .map(|window| RouteWindow {
                    index: window.index,
                    name: window.name.clone(),
                    active: window.active,
                    kind: None,
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    match resolve_route_target(target, &RouteConfig::default(), &route_sessions) {
        RouteResult::Local { target } | RouteResult::SelfNode { target } => target,
        RouteResult::Peer { .. } | RouteResult::Error { .. } => target.to_owned(),
    }
}

fn serveengine_ws_text(text: &str, fallback_target: Option<&str>) -> String {
    serveengine_ws_text_with_runner(text, fallback_target, &ServecoreProcessRunner)
}

fn serveengine_ws_text_with_runner(
    text: &str,
    fallback_target: Option<&str>,
    runner: &dyn ServecoreExecRunner,
) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return serveengine_ws_error("invalid_json", None, None);
    };
    let context_target = value
        .get("target")
        .and_then(serde_json::Value::as_str)
        .or(fallback_target);
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some(action @ ("subscribe" | "select")) => serveengine_ws_target(&value, fallback_target)
            .map_or_else(
                |error| serveengine_ws_error(&error, Some(action), context_target),
                |target| serde_json::json!({"type":"subscribed","target":target}).to_string(),
            ),
        Some("send") => {
            let target = match serveengine_ws_target(&value, fallback_target) {
                Ok(target) => target,
                Err(error) => {
                    return serveengine_ws_error(&error, Some("send"), context_target);
                }
            };
            let Some(text) = value
                .get("text")
                .or_else(|| value.get("content"))
                .and_then(serde_json::Value::as_str)
            else {
                return serveengine_ws_error("missing_text", Some("send"), Some(&target));
            };
            match serveengine_tmux_send(
                &target,
                text,
                value
                    .get("force")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            ) {
                Ok(()) => serde_json::json!({"type":"sent","target":target}).to_string(),
                Err(error) => serveengine_ws_error(&error, Some("send"), Some(&target)),
            }
        }
        Some("sleep") => {
            let target = match serveengine_ws_target(&value, fallback_target) {
                Ok(target) => target,
                Err(error) => {
                    return serveengine_ws_error(&error, Some("sleep"), context_target);
                }
            };
            match serveengine_tmux_run(
                "send-keys",
                &["-t".to_owned(), target.clone(), "C-c".to_owned()],
            ) {
                Ok(_) => serde_json::json!({"type":"slept","target":target}).to_string(),
                Err(error) => serveengine_ws_error(&error, Some("sleep"), Some(&target)),
            }
        }
        Some("wake") => serveengine_ws_wake(&value, fallback_target, runner).map_or_else(
            |error| serveengine_ws_error(&error, Some("wake"), context_target),
            |target| serveengine_woke_reply(&target),
        ),
        Some("restart") => serveengine_ws_restart(&value, fallback_target, runner).map_or_else(
            |error| serveengine_ws_error(&error, Some("restart"), context_target),
            |target| serveengine_restarted_reply(&target),
        ),
        Some("stop") => {
            let target = match serveengine_ws_target(&value, fallback_target) {
                Ok(target) => target,
                Err(error) => {
                    return serveengine_ws_error(&error, Some("stop"), context_target);
                }
            };
            match serveengine_tmux_run("kill-window", &["-t".to_owned(), target.clone()]) {
                Ok(_) => serde_json::json!({"type":"stopped","target":target}).to_string(),
                Err(error) => serveengine_ws_error(&error, Some("stop"), Some(&target)),
            }
        }
        Some(action) => serveengine_ws_error("unsupported_message", Some(action), context_target),
        None => serveengine_ws_error("missing_type", None, context_target),
    }
}

fn serveengine_ws_wake(
    value: &serde_json::Value,
    fallback_target: Option<&str>,
    runner: &dyn ServecoreExecRunner,
) -> Result<String, String> {
    let target = serveengine_ws_target(value, fallback_target)?;
    let argv = serveengine_wake_argv(value, &target);
    let cwd = std::env::current_dir()
        .map_err(|error| format!("serve-ws: current_dir failed: {error}"))?;
    runner.servecore_run(&argv, &cwd)?;
    Ok(target)
}

fn serveengine_ws_restart(
    value: &serde_json::Value,
    fallback_target: Option<&str>,
    runner: &dyn ServecoreExecRunner,
) -> Result<String, String> {
    serveengine_ws_restart_with_sleep(value, fallback_target, runner, std::thread::sleep)
}

fn serveengine_ws_restart_with_sleep(
    value: &serde_json::Value,
    fallback_target: Option<&str>,
    runner: &dyn ServecoreExecRunner,
    mut sleep: impl FnMut(Duration),
) -> Result<String, String> {
    let target = serveengine_ws_target(value, fallback_target)?;
    let interrupt_target = serveengine_resolve_tmux_command_target(&target);
    serveengine_tmux_send_interrupt(&interrupt_target)?;
    sleep(Duration::from_secs(2));
    serveengine_tmux_send_interrupt(&interrupt_target)?;
    sleep(Duration::from_millis(500));
    let argv = serveengine_wake_argv(value, &target);
    let cwd = std::env::current_dir()
        .map_err(|error| format!("serve-ws: current_dir failed: {error}"))?;
    runner.servecore_run(&argv, &cwd)?;
    Ok(target)
}

fn serveengine_resolve_tmux_command_target(target: &str) -> String {
    if std::env::var_os(SERVEENGINE_FAKE_TMUX_LOG_ENV).is_some() {
        return target.to_owned();
    }
    let mut tmux = maw_tmux::TmuxClient::local();
    serveengine_resolve_capture_target(target, &tmux.list_all())
}

fn serveengine_wake_argv(value: &serde_json::Value, target: &str) -> Vec<String> {
    let mut argv = vec![
        "wake".to_owned(),
        target.to_owned(),
        "--no-attach".to_owned(),
        "--yes".to_owned(),
    ];
    if let Some(command) = value.get("command").and_then(serde_json::Value::as_str) {
        argv.extend(["--engine".to_owned(), command.to_owned()]);
    }
    argv
}

fn serveengine_woke_reply(target: &str) -> String {
    serde_json::json!({"type":"woke","target":target}).to_string()
}

fn serveengine_restarted_reply(target: &str) -> String {
    serde_json::json!({"type":"restarted","target":target}).to_string()
}

fn serveengine_ws_target(
    value: &serde_json::Value,
    fallback: Option<&str>,
) -> Result<String, String> {
    value
        .get("target")
        .and_then(serde_json::Value::as_str)
        .or(fallback)
        .ok_or_else(|| "missing_target".to_owned())
        .and_then(serveengine_ws_validate_target)
}

fn serveengine_ws_validate_target(target: &str) -> Result<String, String> {
    ws_validate_target(Some(target))
        .map_err(str::to_owned)?
        .ok_or_else(|| "missing_target".to_owned())
}

fn serveengine_tmux_send(target: &str, text: &str, enter: bool) -> Result<(), String> {
    serveengine_tmux_run(
        "send-keys",
        &maw_tmux::tmux_send_keys_literal_args(target, text),
    )?;
    if enter {
        serveengine_tmux_run("send-keys", &maw_tmux::tmux_send_enter_args(target))?;
    }
    Ok(())
}

fn serveengine_tmux_send_interrupt(target: &str) -> Result<(), String> {
    serveengine_tmux_run(
        "send-keys",
        &["-t".to_owned(), target.to_owned(), "C-c".to_owned()],
    )
    .map(|_| ())
}

fn serveengine_tmux_run(subcommand: &str, args: &[String]) -> Result<String, String> {
    if let Some(log) = std::env::var_os(SERVEENGINE_FAKE_TMUX_LOG_ENV).map(PathBuf::from) {
        let mut body = std::fs::read_to_string(&log).unwrap_or_default();
        body.push_str(&serde_json::json!({"subcommand":subcommand,"args":args}).to_string());
        body.push('\n');
        std::fs::write(log, body)
            .map_err(|error| format!("serve-ws: fake tmux log failed: {error}"))?;
        return Ok(String::new());
    }
    let mut runner = maw_tmux::CommandTmuxRunner::new();
    maw_tmux::TmuxRunner::run(&mut runner, subcommand, args).map_err(|error| error.message)
}

fn serveengine_ws_error(error: &str, action: Option<&str>, target: Option<&str>) -> String {
    let mut frame = serde_json::Map::new();
    frame.insert("type".to_owned(), "error".into());
    frame.insert("error".to_owned(), error.into());
    if let Some(action) = action {
        frame.insert("action".to_owned(), action.into());
    }
    if let Some(target) = target {
        frame.insert("target".to_owned(), target.into());
    }
    serde_json::Value::Object(frame).to_string()
}

pub(crate) async fn serveengine_ws_tmux_stream(
    mut socket: WebSocket,
    state: Arc<ServecoreSharedState>,
    fallback_target: Option<String>,
    config: &WsConfig,
) {
    if !serveengine_ws_tmux_push(&mut socket, &state, fallback_target.as_deref(), config).await {
        return;
    }
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + config.heartbeat_interval,
        config.heartbeat_interval,
    );
    let mut capture_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + config.capture_interval,
        config.capture_interval,
    );
    let idle_timer = tokio::time::sleep(config.idle_timeout);
    tokio::pin!(idle_timer);
    loop {
        tokio::select! {
            _ = capture_tick.tick() => {
                if !serveengine_ws_tmux_push(&mut socket, &state, fallback_target.as_deref(), config).await {
                    break;
                }
                idle_timer.as_mut().reset(tokio::time::Instant::now() + config.idle_timeout);
            }
            _ = heartbeat.tick() => {
                if servecore_ws_send(&mut socket, Message::Ping(Vec::new()), config.send_timeout).await.is_err() {
                    break;
                }
            }
            () = &mut idle_timer => {
                let _ = servecore_ws_send(&mut socket, Message::Close(None), config.send_timeout).await;
                break;
            }
            frame = socket.recv() => match frame {
                Some(Ok(frame)) => {
                    let resets_idle = !matches!(frame, Message::Pong(_));
                    if resets_idle {
                        idle_timer.as_mut().reset(tokio::time::Instant::now() + config.idle_timeout);
                    }
                    if !serveengine_ws_tmux_frame(&mut socket, &state, fallback_target.as_deref(), config, frame).await {
                        break;
                    }
                }
                Some(Err(_)) | None => break,
            }
        }
    }
}

async fn serveengine_ws_tmux_frame(
    socket: &mut WebSocket,
    state: &ServecoreSharedState,
    fallback_target: Option<&str>,
    config: &WsConfig,
    frame: Message,
) -> bool {
    match frame {
        Message::Text(text) => {
            if text.len() > config.max_frame_bytes {
                return false;
            }
            if serveengine_ws_tmux_is_refresh(&text) {
                serveengine_ws_tmux_push(socket, state, fallback_target, config).await
            } else {
                servecore_ws_send(
                    socket,
                    Message::Text(serveengine_ws_error("unsupported_message", None, None)),
                    config.send_timeout,
                )
                .await
                .is_ok()
            }
        }
        Message::Binary(bytes) => bytes.len() <= config.max_frame_bytes,
        Message::Ping(bytes) => {
            servecore_ws_send(socket, Message::Pong(bytes), config.send_timeout)
                .await
                .is_ok()
        }
        Message::Pong(_) => true,
        Message::Close(frame) => {
            let _ = servecore_ws_send(socket, Message::Close(frame), config.send_timeout).await;
            false
        }
    }
}

fn serveengine_ws_tmux_is_refresh(text: &str) -> bool {
    if text == "refresh" {
        return true;
    }
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(|kind| kind == "refresh")
        })
        .unwrap_or(false)
}

async fn serveengine_ws_tmux_push(
    socket: &mut WebSocket,
    state: &ServecoreSharedState,
    fallback_target: Option<&str>,
    config: &WsConfig,
) -> bool {
    let frames = serveengine_ws_tmux_frames(state, fallback_target);
    servecore_ws_send_text_frames(socket, frames, config).await
}

fn serveengine_ws_tmux_frames(
    state: &ServecoreSharedState,
    fallback_target: Option<&str>,
) -> Vec<String> {
    let mut frames = godui_ws_session_recent_frames(state.servecore_tmux_sessions());
    if let Some(target) = fallback_target {
        let frame = match serveengine_tmux_capture(target) {
            Ok(content) => {
                serde_json::json!({"type":"capture","target":target,"content":content}).to_string()
            }
            Err(error) => serde_json::json!({"type":"error","error":error}).to_string(),
        };
        frames.push(frame);
    }
    frames
}

pub(crate) async fn serveengine_ws_pty_stream(
    mut socket: WebSocket,
    state: Arc<ServecoreSharedState>,
    fallback_target: Option<String>,
    config: &WsConfig,
) {
    let mut session: Option<ServeenginePtySession> = None;
    let mut pty_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>> = None;
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + config.heartbeat_interval,
        config.heartbeat_interval,
    );
    let idle_timer = tokio::time::sleep(config.idle_timeout);
    tokio::pin!(idle_timer);
    loop {
        tokio::select! {
            pty = async {
                match pty_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match pty {
                Some(bytes) => {
                    if servecore_ws_send(&mut socket, Message::Binary(bytes), config.send_timeout).await.is_err() {
                        break;
                    }
                    idle_timer.as_mut().reset(tokio::time::Instant::now() + config.idle_timeout);
                }
                None => break,
            },
            _ = heartbeat.tick() => {
                if servecore_ws_send(&mut socket, Message::Ping(Vec::new()), config.send_timeout).await.is_err() {
                    break;
                }
            }
            () = &mut idle_timer => {
                let _ = servecore_ws_send(&mut socket, Message::Close(None), config.send_timeout).await;
                break;
            }
            frame = socket.recv() => match frame {
                Some(Ok(frame)) => {
                    idle_timer.as_mut().reset(tokio::time::Instant::now() + config.idle_timeout);
                    if !serveengine_ws_pty_frame(&mut socket, &state, fallback_target.as_deref(), &mut session, &mut pty_rx, config, frame).await {
                        break;
                    }
                }
                Some(Err(_)) | None => break,
            }
        }
    }
    let _ = servecore_ws_send(
        &mut socket,
        Message::Text(serde_json::json!({"type":"detached"}).to_string()),
        config.send_timeout,
    )
    .await;
    drop(session);
}

async fn serveengine_ws_pty_frame(
    socket: &mut WebSocket,
    state: &ServecoreSharedState,
    fallback_target: Option<&str>,
    session: &mut Option<ServeenginePtySession>,
    pty_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    config: &WsConfig,
    frame: Message,
) -> bool {
    match frame {
        Message::Text(text) => {
            if text.len() > config.max_frame_bytes {
                return false;
            }
            serveengine_ws_pty_text(
                socket,
                state,
                fallback_target,
                session,
                pty_rx,
                config,
                &text,
            )
            .await
        }
        Message::Binary(bytes) => {
            if bytes.len() > config.max_frame_bytes {
                return false;
            }
            session.as_ref().is_none_or(|pty| pty.write(&bytes).is_ok())
        }
        Message::Ping(bytes) => {
            servecore_ws_send(socket, Message::Pong(bytes), config.send_timeout)
                .await
                .is_ok()
        }
        Message::Pong(_) => true,
        Message::Close(frame) => {
            let _ = servecore_ws_send(socket, Message::Close(frame), config.send_timeout).await;
            false
        }
    }
}

async fn serveengine_ws_pty_text(
    socket: &mut WebSocket,
    state: &ServecoreSharedState,
    fallback_target: Option<&str>,
    session: &mut Option<ServeenginePtySession>,
    pty_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    config: &WsConfig,
    text: &str,
) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return servecore_ws_send(
            socket,
            Message::Text(serveengine_ws_error("invalid_json", None, None)),
            config.send_timeout,
        )
        .await
        .is_ok();
    };
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("attach") if session.is_none() => {
            match serveengine_pty_attach(state, &value, fallback_target) {
                Ok((pty, rx)) => {
                    let target = pty.target.clone();
                    *session = Some(pty);
                    *pty_rx = Some(rx);
                    servecore_ws_send(
                        socket,
                        Message::Text(
                            serde_json::json!({"type":"attached","target":target}).to_string(),
                        ),
                        config.send_timeout,
                    )
                    .await
                    .is_ok()
                }
                Err(error) => servecore_ws_send(
                    socket,
                    Message::Text(serveengine_ws_error(&error, None, None)),
                    config.send_timeout,
                )
                .await
                .is_ok(),
            }
        }
        Some("attach") => servecore_ws_send(
            socket,
            Message::Text(serveengine_ws_error("already_attached", None, None)),
            config.send_timeout,
        )
        .await
        .is_ok(),
        Some("resize") => session.as_ref().is_none_or(|pty| {
            let size = serveengine_pty_size(&value);
            pty.master.resize(size).is_ok()
        }),
        Some("detach") => false,
        _ => servecore_ws_send(
            socket,
            Message::Text(serveengine_ws_error("unsupported_message", None, None)),
            config.send_timeout,
        )
        .await
        .is_ok(),
    }
}

struct ServeenginePtySession {
    target: String,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

impl ServeenginePtySession {
    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| "pty_writer_poisoned".to_owned())?;
        writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
            .map_err(|error| format!("pty_write_failed: {error}"))
    }
}

impl Drop for ServeenginePtySession {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

fn serveengine_pty_attach(
    state: &ServecoreSharedState,
    value: &serde_json::Value,
    fallback_target: Option<&str>,
) -> Result<
    (
        ServeenginePtySession,
        tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    ),
    String,
> {
    let target =
        serveengine_pty_resolve_target(state, &serveengine_ws_target(value, fallback_target)?)?;
    let pair = portable_pty::native_pty_system()
        .openpty(serveengine_pty_size(value))
        .map_err(|error| format!("pty_open_failed: {error}"))?;
    let mut child = pair
        .slave
        .spawn_command(serveengine_pty_command(&target))
        .map_err(|error| format!("pty_spawn_failed: {error}"))?;
    drop(pair.slave);
    let killer = child.clone_killer();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("pty_reader_failed: {error}"))?;
    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .map_err(|error| format!("pty_writer_failed: {error}"))?,
    ));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || serveengine_pty_read_loop(reader, &tx));
    Ok((
        ServeenginePtySession {
            target,
            master: pair.master,
            writer,
            killer,
        },
        rx,
    ))
}

fn serveengine_pty_read_loop(
    mut reader: Box<dyn Read + Send>,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) {
    let mut buf = [0_u8; 8192];
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
            break;
        }
    }
}

fn serveengine_pty_size(value: &serde_json::Value) -> PtySize {
    PtySize {
        rows: serveengine_pty_u16(value, "rows", 24),
        cols: serveengine_pty_u16(value, "cols", 80),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn serveengine_pty_u16(value: &serde_json::Value, key: &str, fallback: u16) -> u16 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|raw| u16::try_from(raw).ok())
        .filter(|raw| *raw > 0)
        .unwrap_or(fallback)
}

fn serveengine_pty_resolve_target(
    state: &ServecoreSharedState,
    target: &str,
) -> Result<String, String> {
    let target = serveengine_ws_validate_target(target)?;
    if target.contains(':') {
        return Ok(target);
    }
    for pane in state.servecore_agents_panes() {
        if pane.id == target || pane.title == target {
            return serveengine_ws_validate_target(&pane.target);
        }
    }
    Ok(serveengine_resolve_capture_target(
        &target,
        &state.servecore_tmux_sessions(),
    ))
}

fn serveengine_pty_argv(target: &str) -> Vec<OsString> {
    if let Some(program) = std::env::var_os(SERVEENGINE_PTY_PROGRAM_ENV) {
        return vec![program];
    }
    ["tmux", "attach", "-t", target]
        .into_iter()
        .map(OsString::from)
        .collect()
}

fn serveengine_pty_command(target: &str) -> CommandBuilder {
    let mut command = CommandBuilder::from_argv(serveengine_pty_argv(target));
    // tmux uses screen-256color inside sessions, and it is widely available
    // across macOS and Linux terminfo databases used by cross-node portals.
    command.env("TERM", SERVEENGINE_PTY_TERM);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::OnceLock,
        time::{SystemTime, UNIX_EPOCH},
    };

    // SERVEENGINE_FAKE_TMUX_LOG_ENV is process-global; serialize the tests that set it
    // so parallel cargo-test threads don't clobber each other's env var / log file.
    static FAKE_TMUX_LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn fake_tmux_log_lock() -> &'static Mutex<()> {
        FAKE_TMUX_LOG_LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_os(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old }
        }

        fn set_path(key: &'static str, value: &Path) -> Self {
            Self::set_os(key, value.as_os_str())
        }

        fn set_str(key: &'static str, value: &str) -> Self {
            Self::set_os(key, std::ffi::OsStr::new(value))
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(old) = &self.old {
                std::env::set_var(self.key, old);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let mut path = std::env::temp_dir();
        path.push(format!(
            "maw-rs-serveengine-{name}-{}-{stamp}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp");
        path
    }

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<(Vec<String>, PathBuf)>>,
    }

    impl ServecoreExecRunner for FakeRunner {
        fn servecore_run(&self, argv: &[String], cwd: &Path) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((argv.to_vec(), cwd.to_path_buf()));
            Ok(())
        }
    }

    struct FailingRunner;

    impl ServecoreExecRunner for FailingRunner {
        fn servecore_run(&self, _argv: &[String], _cwd: &Path) -> Result<(), String> {
            Err("serve-orchestration: workon exited with exit status: 2".to_owned())
        }
    }

    #[test]
    fn serveengine_self_bin_uses_env() {
        let root = temp_dir("self-bin");
        let fake = root.join("maw-self");
        fs::write(&fake, "#!/bin/sh\nexit 0\n").expect("fake");
        let _guard = EnvGuard::set_path("MAW_RS_SELF_BIN", &fake);
        assert_eq!(serveengine_self_bin().expect("self bin"), fake);
    }

    #[test]
    fn serveengine_child_timeout_uses_env_override_and_garbage_falls_back() {
        {
            let _guard = EnvGuard::set_str(SERVEENGINE_CHILD_TIMEOUT_ENV, "42");
            assert_eq!(serveengine_child_timeout(), Duration::from_secs(42));
        }
        {
            let _guard = EnvGuard::set_str(SERVEENGINE_CHILD_TIMEOUT_ENV, "nope");
            assert_eq!(
                serveengine_child_timeout(),
                Duration::from_secs(SERVEENGINE_CHILD_TIMEOUT_SECS)
            );
        }
    }

    #[test]
    fn serveengine_pty_command_sets_portable_tmux_term() {
        let command = serveengine_pty_command("demo:1.0");

        assert_eq!(
            command.get_env("TERM"),
            Some(std::ffi::OsStr::new("screen-256color"))
        );
    }

    #[test]
    fn serveengine_runner_reaches_marker_with_argv_and_cwd() {
        let root = temp_dir("marker");
        let bin = root.join("maw-marker");
        let marker = root.join("marker.json");
        fs::write(
            &bin,
            format!(
                r#"#!/bin/sh
printf '{{"cwd":"%s","argv":["%s","%s","%s","%s"]}}' "$(pwd)" "$1" "$2" "$3" "$4" > '{}'
"#,
                marker.display()
            ),
        )
        .expect("script");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).expect("chmod");
        serveengine_run_with_timeout(
            &bin,
            &[
                "workon".to_owned(),
                "demo".to_owned(),
                "--layout".to_owned(),
                "nested".to_owned(),
            ],
            &root,
            serveengine_child_timeout(),
        )
        .expect("run");
        let body = fs::read_to_string(marker).expect("marker");
        assert!(body.contains("\"cwd\""));
        assert!(body.contains("\"workon\""));
        assert!(body.contains("\"--layout\""));
    }

    #[test]
    fn serveengine_timeout_is_generic() {
        let root = temp_dir("timeout");
        let bin = root.join("maw-sleep");
        fs::write(&bin, "#!/bin/sh\n/bin/sleep 2\n").expect("script");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).expect("chmod");
        let err = serveengine_run_with_timeout(&bin, &[], &root, Duration::from_millis(10))
            .expect_err("timeout");
        assert_eq!(err, "serve-orchestration: workon timed out");
    }

    #[test]
    fn serveengine_native_ws_handles_terminal_messages_without_echo() {
        let _lock = fake_tmux_log_lock().lock().expect("lock");
        let root = temp_dir("ws");
        let log = root.join("tmux.jsonl");
        let _guard = EnvGuard::set_path(SERVEENGINE_FAKE_TMUX_LOG_ENV, &log);
        let engine = ServecoreNativeEngine;

        let reply = engine
            .servecore_ws_text(
                ServecoreWsKind::Engine,
                r#"{"type":"subscribe","target":"demo:1"}"#,
                None,
            )
            .expect("reply");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).expect("json")["type"],
            "subscribed"
        );
        let reply = engine
            .servecore_ws_text(
                ServecoreWsKind::Engine,
                r#"{"type":"send","target":"demo:1","text":"ls","force":true}"#,
                None,
            )
            .expect("reply");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).expect("json")["type"],
            "sent"
        );
        let log = fs::read_to_string(log).expect("tmux log");
        assert!(log.contains(r#""send-keys""#));
        assert!(log.contains(r#""ls""#));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn serveengine_native_ws_handles_sleep_and_stop() {
        let _lock = fake_tmux_log_lock().lock().expect("lock");
        let root = temp_dir("ws-sleep-stop");
        let log = root.join("tmux.jsonl");
        let _guard = EnvGuard::set_path(SERVEENGINE_FAKE_TMUX_LOG_ENV, &log);
        let engine = ServecoreNativeEngine;

        let reply = engine
            .servecore_ws_text(
                ServecoreWsKind::Engine,
                r#"{"type":"sleep","target":"demo:1"}"#,
                None,
            )
            .expect("reply");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).expect("json")["type"],
            "slept"
        );

        let reply = engine
            .servecore_ws_text(
                ServecoreWsKind::Engine,
                r#"{"type":"stop","target":"demo:1"}"#,
                None,
            )
            .expect("reply");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).expect("json")["type"],
            "stopped"
        );

        let log = fs::read_to_string(log).expect("tmux log");
        assert!(log.contains(r#""send-keys""#));
        assert!(log.contains(r#""C-c""#));
        assert!(log.contains(r#""kill-window""#));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn serveengine_native_ws_wake_uses_self_runner_and_engine_flag() {
        let runner = FakeRunner::default();
        let value = serde_json::json!({
            "type": "wake",
            "target": "demo:1",
            "command": "codex",
        });

        let target = serveengine_ws_wake(&value, None, &runner).expect("wake");

        assert_eq!(target, "demo:1");
        let calls = runner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            vec![
                "wake",
                "demo:1",
                "--no-attach",
                "--yes",
                "--engine",
                "codex",
            ]
        );
        assert!(calls[0].1.is_dir());
    }

    #[test]
    fn serveengine_native_ws_wake_error_includes_action_and_target() {
        let reply = serveengine_ws_text_with_runner(
            r#"{"type":"wake","target":"ws-parity-missing:999"}"#,
            None,
            &FailingRunner,
        );

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).expect("json"),
            serde_json::json!({
                "type": "error",
                "error": "serve-orchestration: workon exited with exit status: 2",
                "action": "wake",
                "target": "ws-parity-missing:999",
            })
        );
    }

    #[test]
    fn serveengine_native_ws_context_free_error_omits_action_and_target() {
        let reply = serveengine_ws_text("not-json", None);

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).expect("json"),
            serde_json::json!({"type": "error", "error": "invalid_json"})
        );
    }

    #[test]
    fn serveengine_native_ws_restart_interrupts_twice_then_wakes() {
        let _lock = fake_tmux_log_lock().lock().expect("lock");
        let root = temp_dir("ws-restart");
        let log = root.join("tmux.jsonl");
        let _guard = EnvGuard::set_path(SERVEENGINE_FAKE_TMUX_LOG_ENV, &log);
        let runner = FakeRunner::default();
        let value = serde_json::json!({
            "type": "restart",
            "target": "demo:1",
            "command": "claude",
        });
        let mut sleeps = Vec::new();

        let target =
            serveengine_ws_restart_with_sleep(&value, None, &runner, |delay| sleeps.push(delay))
                .expect("restart");

        assert_eq!(target, "demo:1");
        assert_eq!(
            sleeps,
            vec![Duration::from_secs(2), Duration::from_millis(500)]
        );
        let log = fs::read_to_string(log).expect("tmux log");
        let events = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json"))
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|event| event["subcommand"] == "send-keys"));
        assert!(events.iter().all(|event| event["args"][2] == "C-c"));
        let calls = runner
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            calls[0].0,
            vec![
                "wake",
                "demo:1",
                "--no-attach",
                "--yes",
                "--engine",
                "claude",
            ]
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn serveengine_native_ws_sleep_and_stop_reject_bad_target() {
        let engine = ServecoreNativeEngine;

        let reply = engine
            .servecore_ws_text(
                ServecoreWsKind::Engine,
                r#"{"type":"sleep","target":"bad;name"}"#,
                None,
            )
            .expect("reply");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).expect("json")["type"],
            "error"
        );

        let reply = engine
            .servecore_ws_text(ServecoreWsKind::Engine, r#"{"type":"stop"}"#, None)
            .expect("reply");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply).expect("json")["type"],
            "error"
        );
    }

    #[test]
    fn serveengine_short_capture_lines_match_maw_js_tail_behavior() {
        let capture = "one\ntwo\nthree\nfour\n".to_owned();

        assert_eq!(
            serveengine_tail_capture_lines(capture.clone(), Some(2)),
            "three\nfour\n"
        );
        assert_eq!(
            serveengine_tail_capture_lines(capture.clone(), Some(80)),
            capture
        );
        assert_eq!(
            serveengine_tail_capture_lines(capture.clone(), None),
            capture
        );
    }

    #[test]
    fn serveengine_capture_resolves_bare_window_names() {
        let sessions = vec![TmuxSession {
            name: "35-fable-learn-speckit".to_owned(),
            windows: vec![maw_tmux::TmuxWindow {
                index: 2,
                name: "fable-codex-2".to_owned(),
                active: false,
                cwd: None,
            }],
        }];

        assert_eq!(
            serveengine_resolve_capture_target("fable-codex-2", &sessions),
            "35-fable-learn-speckit:2"
        );
    }
}
