const DISPATCH_307: &[DispatcherEntry] = &[
    DispatcherEntry { command: "hey", handler: Handler::Async(run_hey_async) },
    DispatcherEntry { command: "send", handler: Handler::Async(run_send_async) },
    DispatcherEntry { command: "health", handler: Handler::Async(run_health_async) },
    DispatcherEntry { command: "reply", handler: Handler::Async(run_reply_async) },
    DispatcherEntry { command: "rp", handler: Handler::Async(run_reply_async) },
];

#[derive(Debug, Clone, Default)]
struct SendArgs {
    target: String,
    text: String,
    inbox: Option<bool>,
    from: Option<String>,
    approve: bool,
    trust: bool,
    dry_run: bool,
}

// Where the message text comes from (#528): positional argv (historical), a
// file via `-f <path>`, or stdin via a literal `-`. File/stdin content never
// passes through a shell, so backticks/$/quotes/newlines stay inert.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum SendMessageSource {
    #[default]
    Positional,
    File(String),
    Stdin,
}


#[derive(Debug, Clone, Default)]
struct HeyConfig {
    node: Option<String>,
    oracle: Option<String>,
    route: RouteConfig,
}


fn run_hey_async(args: Vec<String>) -> Pin<Box<dyn Future<Output = CliOutput> + Send>> {
    Box::pin(async move {
        if args.first().is_some_and(|arg| arg == "log") { return hey_log_command(&args[1..]); }
        run_send_like_async_impl("hey", &args).await
    })
}

fn run_send_async(args: Vec<String>) -> Pin<Box<dyn Future<Output = CliOutput> + Send>> {
    Box::pin(async move { run_send_like_async_impl("send", &args).await })
}


async fn run_send_like_async_impl(command: &str, raw_args: &[String]) -> CliOutput {
    if wants_help_before_positionals(raw_args, &["--from", "-f"]) {
        return help_output(send_usage(command));
    }
    let send_args = match parse_send_args(command, raw_args) {
        Ok(parsed) => parsed,
        Err(message) => return send_usage_error(command, &message),
    };
    let audit_args = send_audit_args(command, raw_args);
    run_send_like_async_with_args(command, send_args, false, audit_args).await
}

async fn run_hey_in_process(query: &str, message: &str, acl_bypass: bool) -> CliOutput {
    let send_args = send_args_for_inbox_hey(query, message);
    run_send_like_async_with_args("hey", send_args, acl_bypass, vec!["hey".to_owned(), query.to_owned(), message.to_owned()]).await
}

fn send_args_for_inbox_hey(query: &str, message: &str) -> SendArgs {
    SendArgs {
        target: query.to_owned(),
        text: message.to_owned(),
        inbox: None,
        from: None,
        approve: false,
        trust: false,
        dry_run: false,
    }
}

async fn run_send_like_async_with_args(
    command: &str,
    send_args: SendArgs,
    acl_bypass: bool,
    audit_args: Vec<String>,
) -> CliOutput {
    let config = load_hey_config();
    let sender_oracle = resolve_hey_sender_oracle_for_from(&config, send_args.from.as_deref());
    let mut tmux = TmuxClient::local();
    let sessions = route_sessions_from_tmux(&mut tmux);
    let routing_target = if command == "hey" {
        match hey_picker_target(&send_args.target, &config.route, &sessions) {
            Ok(target) => target,
            Err(output) => return output,
        }
    } else {
        send_args.target.clone()
    };
    let mut runner = maw_tmux::CommandTmuxRunner::new();
    let result = resolve_send_route_target(
        &routing_target,
        &config.route,
        &sessions,
        std::env::var_os("TMUX").is_some(),
        &mut runner,
    );
    let result =
        route_result_prefer_pane_zero_for_ambiguous_agent(&send_args.target, result, &mut runner);
    if send_args.dry_run {
        return send_dry_run_output(command, &send_args, &result);
    }
    if let Some(refusal) = send_route_gate(command, &send_args.target, &send_args.text, &result) {
        return refusal;
    }
    let feed_id = send_message_feed_id();
    match result {
        RouteResult::Local { target } | RouteResult::SelfNode { target } if send_args.inbox == Some(true) => {
            send_local_inbox_only(
                command,
                &send_args.target,
                &target,
                &send_args.text,
                &config,
                &sender_oracle,
                send_args.from.as_deref(),
                &feed_id,
            )
        }
        RouteResult::Local { target } | RouteResult::SelfNode { target } => send_local_message_with_audit(
            command,
            &mut tmux,
            &target,
            &send_args.target,
            &send_args.text,
            &config,
            &sender_oracle,
            send_args.from.as_deref(),
            &audit_args,
            &feed_id,
        ),
        RouteResult::Peer {
            peer_url,
            target,
            node,
        } => {
            gated_send_peer_message_with_audit(
                command,
                &peer_url,
                &target,
                &node,
                &send_args,
                &config,
                &sender_oracle,
                &audit_args,
                acl_bypass,
            )
            .await
        }
        RouteResult::Error { detail, hint, .. } => CliOutput {
            code: send_error_code(command),
            stdout: String::new(),
            stderr: send_route_error(command, &send_args.target, &detail, hint.as_deref()),
        },
    }
}



















fn parse_send_args(command: &str, argv: &[String]) -> Result<SendArgs, String> {
    parse_send_args_with_stdin(command, argv, || std::io::stdin().lock())
}

fn parse_send_args_with_stdin<R: std::io::Read, F: FnOnce() -> R>(
    command: &str,
    argv: &[String],
    stdin: F,
) -> Result<SendArgs, String> {
    let mut inbox = None;
    let mut from = None;
    let mut positional = Vec::new();
    let mut approve = false;
    let mut trust = false;
    let mut dry_run = false;
    let mut source = SendMessageSource::Positional;
    let mut index = 0;
    while index < argv.len() {
        match argv[index].as_str() {
            "--inbox" => inbox = Some(true),
            "--no-inbox" => inbox = Some(false),
            "--approve" => approve = true,
            "--trust" => trust = true,
            "--dry-run" => dry_run = true,
            "--from" => {
                let Some(value) = argv.get(index + 1) else {
                    return Err(format!("{command}: missing --from value"));
                };
                from = Some(value.clone());
                index += 1;
            }
            value if value.starts_with("--from=") => {
                from = Some(value["--from=".len()..].to_owned());
            }
            "-f" => {
                let Some(value) = argv.get(index + 1) else {
                    return Err(format!("{command}: missing -f value (path to message file)"));
                };
                send_set_message_source(command, &mut source, SendMessageSource::File(value.clone()))?;
                index += 1;
            }
            "-" => send_set_message_source(command, &mut source, SendMessageSource::Stdin)?,
            value if value.starts_with('-') => return Err(format!("{command}: unknown argument {value}")),
            value => positional.push(value.to_owned()),
        }
        index += 1;
    }
    if trust && !approve {
        return Err(format!("{command}: --trust requires --approve"));
    }
    if positional.is_empty() {
        return Err(format!("{command}: target and message are required"));
    }
    let text = match &source {
        SendMessageSource::Positional => {
            if positional.len() == 1 {
                return Err(format!("{command}: missing message for '{}'", positional[0]));
            }
            positional[1..].join(" ")
        }
        SendMessageSource::File(_) | SendMessageSource::Stdin => {
            if positional.len() > 1 {
                return Err(format!(
                    "{command}: message given both as argument and via {}; use exactly one",
                    send_message_source_label(&source)
                ));
            }
            resolve_send_message_source(command, &positional[0], &source, stdin)?
        }
    };
    Ok(SendArgs {
        target: positional[0].clone(),
        text,
        inbox,
        from,
        approve,
        trust,
        dry_run,
    })
}

fn send_message_source_label(source: &SendMessageSource) -> &'static str {
    match source {
        SendMessageSource::Positional => "positional message",
        SendMessageSource::File(_) => "-f <file>",
        SendMessageSource::Stdin => "'-' (stdin)",
    }
}

fn send_set_message_source(
    command: &str,
    slot: &mut SendMessageSource,
    next: SendMessageSource,
) -> Result<(), String> {
    if *slot != SendMessageSource::Positional {
        return Err(format!(
            "{command}: message can come from only one of -f <file> or '-' (stdin)"
        ));
    }
    *slot = next;
    Ok(())
}

// Resolve message text from a file or stdin source (#528). Bytes pass through
// untouched (no shell, no word-splitting, no substitution); non-UTF-8 or
// unreadable input errors name the source. Empty content reuses the exact
// empty-message error positional hey produces today.
fn resolve_send_message_source<R: std::io::Read, F: FnOnce() -> R>(
    command: &str,
    target: &str,
    source: &SendMessageSource,
    stdin: F,
) -> Result<String, String> {
    let content = match source {
        SendMessageSource::Positional => String::new(),
        SendMessageSource::File(path) => {
            let file = std::fs::File::open(path)
                .map_err(|error| format!("{command}: cannot read message file '{path}': {error}"))?;
            send_message_from_reader(command, &format!("file '{path}'"), file)?
        }
        SendMessageSource::Stdin => send_message_from_reader(command, "stdin", stdin())?,
    };
    send_require_nonempty_message(command, target, content)
}

fn send_message_from_reader(
    command: &str,
    label: &str,
    reader: impl std::io::Read,
) -> Result<String, String> {
    std::io::read_to_string(reader)
        .map_err(|error| format!("{command}: cannot read message from {label}: {error}"))
}

fn send_require_nonempty_message(command: &str, target: &str, content: String) -> Result<String, String> {
    if content.trim().is_empty() {
        return Err(format!("{command}: missing message for '{target}'"));
    }
    Ok(content)
}

fn send_audit_args(command: &str, raw_args: &[String]) -> Vec<String> {
    std::iter::once(command.to_owned()).chain(raw_args.iter().cloned()).collect()
}

fn send_usage_error(command: &str, message: &str) -> CliOutput {
    if command == "hey" {
        if message == "hey: target and message are required" {
            return CliOutput { code: 1, stdout: String::new(), stderr: format!("{}\n", send_usage(command)) };
        }
        if let Some(target) = message.strip_prefix("hey: missing message for '").and_then(|message| message.strip_suffix('\'')) {
            return CliOutput {
                code: 1,
                stdout: String::new(),
                stderr: format!("✗ missing message for target '{target}'\n  maw hey {target} <message>\n  (if '{target}' isn't a valid target, run 'maw ls' to see available ones)\n"),
            };
        }
    }
    CliOutput {
        code: send_error_code(command),
        stdout: String::new(),
        stderr: format!("{message}\n{}\n", send_usage(command)),
    }
}

fn send_usage(command: &str) -> String {
    if command == "hey" {
        return "usage: maw hey <target> <message> [--inbox] [--force deprecated] [--approve] [--trust]\n       maw hey <target> -f <file>   read message from file (bytes-through; no shell)\n       maw hey <target> -           read message from stdin\n  default: write receiver inbox and inject into the target pane\n  --inbox: write receiver inbox only; skip pane injection\n  --force: deprecated compatibility alias; delivery is already forced by default\n  target forms:\n    <oracle-window>              same-node window name (local-only)\n    local:<agent>                explicit same-node target\n    <session>:<window>[.<pane>]  paste a TARGET from maw ls -v\n    <node>:<session>             canonical cross-node form (window 1)\n    <node>:<session>:<window>    target a specific tmux window (#410)\n  e.g. maw hey mawjs-oracle \"hello from neo\"\n       maw hey local:mawjs \"hello from neo\"\n       maw hey phaith:01-hojo:3 \"hello hojo-hermes\"\n       run `maw locate <agent>` to enumerate across federation".to_owned();
    }
    format!(
        "usage: maw-rs {command} <target> <message> [--inbox|--no-inbox] [--from <oracle:node>] [--approve] [--trust] [--dry-run]\n       maw-rs {command} <target> -f <file> | -   read message from file or stdin (no shell interpolation)"
    )
}

fn send_error_code(command: &str) -> i32 { if command == "hey" { 1 } else { 2 } }

fn send_route_error(command: &str, query: &str, detail: &str, hint: Option<&str>) -> String {
    if command == "hey" {
        if !query.is_empty() && !query.contains(':') && !query.contains('/') {
            return format!("error: bare target '{query}' not found locally\n\n  same-node targets:\n    maw hey local:{query} \"...\"\n    or copy a TARGET from `maw ls -v`\n\n  cross-node targets:\n    maw hey <node>:{query} \"...\"\n    maw hey <node>:<session>:<window> \"...\"\n\n  bare names are local-only; run `maw locate {query}` to enumerate federation candidates\n");
        }
        let hint = hint.map_or_else(String::new, |hint| format!("hint:  {hint}\n"));
        return format!("error: {detail}\n{hint}");
    }
    hint.map_or_else(|| format!("{command}: {detail}\n"), |hint| format!("{command}: {detail}; {hint}\n"))
}

















fn send_dry_run_output(command: &str, args: &SendArgs, result: &RouteResult) -> CliOutput {
    match result {
        RouteResult::Local { target } => CliOutput {
            code: 0,
            stdout: format!("dry-run: {command} {} -> local {target}\n", args.target),
            stderr: String::new(),
        },
        RouteResult::SelfNode { target } => CliOutput {
            code: 0,
            stdout: format!("dry-run: {command} {} -> self-node {target}\n", args.target),
            stderr: String::new(),
        },
        RouteResult::Peer {
            peer_url,
            target,
            node,
        } => CliOutput {
            code: 0,
            stdout: format!(
                "dry-run: {command} {} -> peer {node} {target} via {peer_url}\n",
                args.target
            ),
            stderr: String::new(),
        },
        RouteResult::Error { detail, hint, .. } => CliOutput {
            code: send_error_code(command),
            stdout: String::new(),
            stderr: send_route_error(command, &args.target, detail, hint.as_deref()),
        },
    }
}



fn send_local_message(
    command: &str,
    tmux: &mut TmuxClient<maw_tmux::CommandTmuxRunner>,
    target: &str,
    text: &str,
    config: &HeyConfig,
    sender_oracle: &str,
    from: Option<&str>,
) -> CliOutput {
    let feed_id = send_message_feed_id();
    send_local_message_with_audit(command, tmux, target, target, text, config, sender_oracle, from, &[], &feed_id)
}

#[allow(clippy::too_many_arguments)]
fn send_local_message_with_audit(
    command: &str,
    tmux: &mut TmuxClient<maw_tmux::CommandTmuxRunner>,
    target: &str,
    query: &str,
    text: &str,
    config: &HeyConfig,
    sender_oracle: &str,
    from: Option<&str>,
    audit_args: &[String],
    feed_id: &str,
) -> CliOutput {
    let signature = match send_message_signature(config, sender_oracle, from, text) {
        Ok(signature) => signature,
        Err(message) => return CliOutput { code: send_error_code(command), stdout: String::new(), stderr: format!("{command}: {message}\n") },
    };
    let display_from = send_display_from(from);
    let outbound = format_local_hey_message(text, config, sender_oracle, display_from.as_deref());
    let send_report = match tmux.send_text(target, &outbound) {
        Ok(report) => report,
        Err(error) => {
            return CliOutput {
                code: 1,
                stdout: String::new(),
                stderr: format!("{command}: tmux send-text failed: {error}\n"),
            };
        }
    };
    send_record_success(command, audit_args, config, sender_oracle, from, query, &outbound, "local", signature.as_ref());
    send_emit_message_lifecycle_feed(&SendMessageLifecycleFeedInput {
        id: feed_id,
        ts: cli_dispatch_now_iso(),
        direction: "outbound",
        state: "delivered",
        channel: command,
        route: "local",
        from: send_normalized_from(config, sender_oracle, from).as_deref().unwrap_or("local:unknown"),
        to: query,
        target: Some(target),
        text: &outbound,
        last_line: None,
        signed: true,
    });
    let pending_warning = send_pending_input_warning(target, &send_report);
    if let Some(warning) = pending_warning.as_deref() {
        send_emit_message_lifecycle_feed(&SendMessageLifecycleFeedInput {
            id: feed_id,
            ts: cli_dispatch_now_iso(),
            direction: "outbound",
            state: "delivered-pending-input",
            channel: command,
            route: "local",
            from: send_normalized_from(config, sender_oracle, from).as_deref().unwrap_or("local:unknown"),
            to: query,
            target: Some(target),
            text: &outbound,
            last_line: Some(warning),
            signed: true,
        });
    }
    // #709: `delivered` must not stay silent when the resolved pane isn't
    // agent-shaped (window rename, pane replaced, agent closed) -- warn
    // rather than let a shell-prompt delivery look identical to a real one.
    let mut stderr = String::new();
    if let Some(warning) = pending_warning {
        stderr.push_str(&send_warning_line(&warning));
    }
    if let Some(warning) = serve_non_agent_pane_warning(target) {
        stderr.push_str(&send_warning_line(&warning));
    }
    CliOutput {
        code: 0,
        stdout: send_success_output(command, target, &outbound),
        stderr,
    }
}

/// `--inbox` on a local target must write the receiver's `ψ/inbox`, not
/// inject into the pane (#672 defect 2) — `send_local_message_with_audit`
/// takes no `inbox` parameter and always injects, so this is a separate
/// path taken before that function is ever reached. Mirrors `notify_local_with`
/// (same receiver-repo resolution via the registry, same `inbox_write_file`),
/// so the two durable-write commands stay consistent instead of diverging.
#[allow(clippy::too_many_arguments)]
fn send_local_inbox_only(
    command: &str,
    query: &str,
    target: &str,
    text: &str,
    config: &HeyConfig,
    sender_oracle: &str,
    from: Option<&str>,
    feed_id: &str,
) -> CliOutput {
    send_local_inbox_only_with(command, query, target, text, config, sender_oracle, from, &locate_find_oracle_repo_path, feed_id)
}

#[allow(clippy::too_many_arguments)]
fn send_local_inbox_only_with(
    command: &str,
    query: &str,
    target: &str,
    text: &str,
    config: &HeyConfig,
    sender_oracle: &str,
    from: Option<&str>,
    resolve_repo: &dyn Fn(&str) -> Option<String>,
    feed_id: &str,
) -> CliOutput {
    let to = notify_inbox_to(query, target);
    let Some(repo_path) = resolve_repo(&to) else {
        return CliOutput {
            code: send_error_code(command),
            stdout: String::new(),
            stderr: format!(
                "{command}: cannot resolve a local inbox for '{to}'; not a known local oracle — check `maw locate {to} --path`\n"
            ),
        };
    };
    let display_from = notify_display_from(from, config, sender_oracle);
    let inbox_dir = std::path::Path::new(&repo_path).join("ψ").join("inbox");
    match inbox_write_file(&inbox_dir, &display_from, &to, text, inbox_now_ms()) {
        Ok(filename) => {
            send_emit_message_lifecycle_feed(&SendMessageLifecycleFeedInput {
                id: feed_id,
                ts: cli_dispatch_now_iso(),
                direction: "outbound",
                state: "queued",
                channel: command,
                route: "inbox",
                from: display_from.as_str(),
                to: query,
                target: Some(target),
                text,
                last_line: Some("--inbox requested; pane injection skipped"),
                signed: true,
            });
            CliOutput {
                code: 0,
                stdout: format!("queued inbox {to} {filename}\n"),
                stderr: String::new(),
            }
        }
        Err(message) => {
            CliOutput { code: 1, stdout: String::new(), stderr: format!("{command}: {message}\n") }
        }
    }
}

fn send_success_output(command: &str, target: &str, outbound: &str) -> String {
    if command == "hey" { format!("delivered → {target}: {outbound}\n") } else { format!("delivered {target}\n") }
}

fn send_pending_input_warning(
    target: &str,
    report: &maw_tmux::SendTextReport,
) -> Option<String> {
    report.warned_pending.then(|| {
        format!(
            "delivered to {target}, but the pane may still have unsubmitted input after {} Enter attempt(s); run `maw send-enter {target}` to submit it manually",
            report.enter_attempts
        )
    })
}

fn send_warning_line(warning: &str) -> String {
    format!("\x1b[33mwarning: {warning}\x1b[0m\n")
}

struct SendMessageLifecycleFeedInput<'a> {
    id: &'a str,
    ts: String,
    direction: &'a str,
    state: &'a str,
    channel: &'a str,
    route: &'a str,
    from: &'a str,
    to: &'a str,
    target: Option<&'a str>,
    text: &'a str,
    last_line: Option<&'a str>,
    signed: bool,
}

fn send_message_feed_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::from("msg-");
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn send_emit_message_lifecycle_feed(input: &SendMessageLifecycleFeedInput<'_>) {
    let event = send_build_message_lifecycle_feed_event(input);
    let port = load_hey_config_port().unwrap_or(3456);
    send_emit_feed_fire_and_forget(port, &event);
}

fn send_build_message_lifecycle_feed_event(input: &SendMessageLifecycleFeedInput<'_>) -> serde_json::Value {
    let data = send_build_message_lifecycle_data(input);
    let (host, oracle) = send_parse_message_feed_identity(if input.direction == "inbound" { input.to } else { input.from });
    let timestamp = input.ts.clone();
    let ts = send_message_feed_ts_millis(&timestamp);
    let event = send_message_feed_event_type(input.direction, input.state);
    let message = send_message_feed_summary(input);
    serde_json::json!({
        "timestamp": timestamp,
        "oracle": oracle,
        "host": host,
        "event": event,
        "project": "",
        "sessionId": input.target.unwrap_or_default(),
        "message": message,
        "ts": ts,
        "data": data,
    })
}

fn send_build_message_lifecycle_data(input: &SendMessageLifecycleFeedInput<'_>) -> serde_json::Value {
    let mut data = serde_json::json!({
        "id": input.id,
        "ts": input.ts,
        "direction": input.direction,
        "state": input.state,
        "channel": input.channel,
        "route": input.route,
        "from": input.from,
        "to": input.to,
        "text": input.text,
        "signed": input.signed,
    });
    if let Some(target) = input.target {
        data["target"] = serde_json::json!(target);
    }
    if let Some(last_line) = input.last_line {
        data["lastLine"] = serde_json::json!(last_line);
    }
    data
}

fn send_parse_message_feed_identity(value: &str) -> (String, String) {
    if let Some((host, oracle)) = value.split_once(':') {
        if !host.is_empty() && !oracle.is_empty() {
            return (host.to_owned(), oracle.to_owned());
        }
    }
    ("local".to_owned(), if value.is_empty() { "unknown" } else { value }.to_owned())
}

fn send_message_feed_event_type(direction: &str, state: &str) -> &'static str {
    if state == "failed" {
        "MessageFail"
    } else if direction == "outbound" {
        "MessageSend"
    } else {
        "MessageDeliver"
    }
}

fn send_message_feed_ts_millis(timestamp: &str) -> u64 {
    let base = inbox_parse_iso_ms(timestamp).unwrap_or_else(cli_dispatch_now_millis);
    let seconds = timestamp.get(17..19).and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
    let millis = timestamp
        .split_once('.')
        .and_then(|(_, tail)| tail.get(..tail.len().min(3)))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    base.saturating_add(seconds.saturating_mul(1_000)).saturating_add(millis)
}

fn send_message_feed_summary(input: &SendMessageLifecycleFeedInput<'_>) -> String {
    let mut parts = vec![
        format!("{}/{}", input.direction, input.state),
        format!("{} → {}", input.from, input.to),
    ];
    if let Some(target) = input.target {
        parts.push(format!("({target})"));
    }
    parts.push(input.text.chars().take(200).collect());
    parts.join(" ")
}

fn send_emit_feed_fire_and_forget(port: u16, event: &serde_json::Value) {
    let Ok(body) = serde_json::to_string(&event) else {
        return;
    };
    let _ = send_post_feed_once(port, &body);
}

fn send_post_feed_once(port: u16, body: &str) -> std::io::Result<()> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = std::time::Duration::from_millis(150);
    let mut stream = std::net::TcpStream::connect_timeout(&addr, timeout)?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request = format!(
        "POST /api/feed HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    let mut response = [0_u8; 1];
    let _ = std::io::Read::read(&mut stream, &mut response);
    Ok(())
}

/// An empty message body must never reach delivery (#695): a caller with no
/// text to send gets a refusal here, not a `[node:sender]` tag arriving in a
/// pane with nothing after it — which is indistinguishable from a real
/// message that failed to type, and burns a turn for whatever reads the pane.
fn send_empty_body_output(command: &str, text: &str) -> Option<CliOutput> {
    if text.trim().is_empty() {
        return Some(CliOutput {
            code: send_error_code(command),
            stdout: String::new(),
            stderr: format!("{command}: refusing to deliver an empty message body\n"),
        });
    }
    None
}

/// The single decision point `run_send_like_async_with_args` consults before
/// any delivery path runs (#695): refuses an empty body regardless of route,
/// and refuses a `SelfNode` route outright. Kept as one pure function, tested
/// directly against constructed `RouteResult` values, so the wiring itself is
/// pinned — not just the message text each refusal produces.
fn send_route_gate(command: &str, query: &str, text: &str, result: &RouteResult) -> Option<CliOutput> {
    if let Some(refusal) = send_empty_body_output(command, text) {
        return Some(refusal);
    }
    if let RouteResult::SelfNode { target } = result {
        if !send_query_uses_explicit_local_prefix(query) {
            return Some(send_self_node_refusal_output(command, query, target));
        }
    }
    None
}

/// `maw-routing` resolves the documented `local:<agent>` form (an explicit
/// same-node target — see the `hey` usage text) through the SAME `SelfNode`
/// branch as a real cross-node alias that happens to equal this node's own
/// identity: both take the `node_name == self_node || node_name == "local"`
/// path in `resolve_target_with_current_session`. Only the second shape is
/// the #695 loopback-self bug; `local:` is normal, heavily-used, intentional
/// routing and must keep working. `ResolveResult`/`RouteResult` carries no
/// discriminant between the two (both are just `SelfNode { target }`), so
/// the query string itself is the only signal available client-side.
fn send_query_uses_explicit_local_prefix(query: &str) -> bool {
    query.split_once(':').is_some_and(|(node, _)| node == "local")
}

/// `RouteResult::SelfNode` fires only when a query used the full cross-node
/// `<node>:<agent>` form and `<node>` happened to name this very node (#695)
/// — a shape that looks like a caller addressing a peer that turned out to
/// be itself, not an intentional local/self-window send (`hey me` and bare
/// local targets resolve as plain `Local` and are unaffected). Refuse rather
/// than inject: a message quietly delivered to yourself under a peer address
/// is the same "sender == receiver" symptom #695 reported, not a feature.
fn send_self_node_refusal_output(command: &str, query: &str, target: &str) -> CliOutput {
    CliOutput {
        code: send_error_code(command),
        stdout: String::new(),
        stderr: format!(
            "{command}: refusing to deliver — '{query}' addresses this node via its own cross-node name and resolved back to a local pane ({target}); use a plain local target instead\n"
        ),
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_peer_message(
    command: &str,
    peer_url: &str,
    target: &str,
    node: &str,
    args: &SendArgs,
    config: &HeyConfig,
    sender_oracle: &str,
    audit_args: &[String],
) -> CliOutput {
    let from = match resolve_hey_wire_from(args.from.as_deref(), config, sender_oracle) {
        Ok(from) => from,
        Err(message) => {
            return CliOutput {
                code: send_error_code(command),
                stdout: String::new(),
                stderr: format!("{command}: {message}\n"),
            }
        }
    };
    let signature = match send_message_signature(config, sender_oracle, args.from.as_deref(), &args.text) {
        Ok(signature) => signature,
        Err(message) => return CliOutput { code: send_error_code(command), stdout: String::new(), stderr: format!("{command}: {message}\n") },
    };
    let peer_key = match load_peer_key() {
        Ok(key) => key,
        Err(message) => {
            return CliOutput {
                code: 1,
                stdout: String::new(),
                stderr: format!("{command}: {message}\n"),
            }
        }
    };
    let federation_token = match load_federation_token() {
        Ok(token) => token,
        Err(message) => {
            return CliOutput {
                code: 1,
                stdout: String::new(),
                stderr: format!("{command}: {message}\n"),
            }
        }
    };
    let client = match ReqwestHttpTransportIo::new(5_000) {
        Ok(client) => client,
        Err(message) => {
            return CliOutput {
                code: 1,
                stdout: String::new(),
                stderr: format!("{command}: {message}\n"),
            }
        }
    };
    let request = PeerSendRequest {
        peer_url: peer_url.to_owned(),
        target: target.to_owned(),
        text: args.text.clone(),
        inbox: args.inbox,
        from,
        federation_token,
        peer_key,
        timestamp: i64::try_from(current_epoch_seconds()).unwrap_or(i64::MAX),
    };
    match client.send_peer(&request).await {
        Ok(response) => {
            let display_from = send_display_from(args.from.as_deref());
            let outbound = format_local_hey_message(&args.text, config, sender_oracle, display_from.as_deref());
            send_record_success(command, audit_args, config, sender_oracle, args.from.as_deref(), &args.target, &outbound, &format!("peer:{node}"), signature.as_ref());
            // #709: the receiving serve may have delivered into a pane that
            // is not agent-shaped -- surface that here too, not just on the
            // local delivery path, since this is the exact shape m5's field
            // repro hit (a cross-node `hey` landing in a bash prompt).
            let stderr = response
                .warning
                .as_deref()
                .map(|warning| format!("\x1b[33mwarning: {warning}\x1b[0m\n"))
                .unwrap_or_default();
            CliOutput {
                code: 0,
                stdout: format!(
                    "{} {}\n",
                    response.state.as_deref().unwrap_or("queued"),
                    response.target.as_deref().unwrap_or(target)
                ),
                stderr,
            }
        },
        Err(message) => CliOutput {
            code: 1,
            stdout: String::new(),
            stderr: format!("{command}: {message}\n"),
        },
    }
}


#[allow(clippy::too_many_arguments)]
fn send_record_success(
    command: &str,
    audit_args: &[String],
    config: &HeyConfig,
    sender_oracle: &str,
    from: Option<&str>,
    to: &str,
    msg: &str,
    route: &str,
    signature: Option<&MessageSignature>,
) {
    if audit_args.is_empty() {
        return;
    }
    let normalized_from = send_normalized_from(config, sender_oracle, from);
    let record = MessageSinkRecord {
        command,
        audit_args,
        normalized_from: normalized_from.as_deref(),
        sender_oracle,
        to,
        msg,
        route,
        signature,
    };
    for sink in message_sink_registry() {
        sink.record(&record);
    }
}


















































