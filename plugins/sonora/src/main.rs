#![forbid(unsafe_code)]
// Release plugin executables must not allocate a console on Windows; the
// host launches them with CREATE_NO_WINDOW and talks over piped stdio.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

//! TikTools control for a running Sonora.
//!
//! Sonora exposes the same line-oriented control socket its own CLI talks to
//! (`sonora.sock`, protocol 2): one request line in, one reply line out. Each
//! action maps to a single request, and the plugin is deliberately small:
//!
//! - `sonora.search-play` -> `{"queue":{"play":[{"search":{...}}]}}`
//! - `sonora.search-add` -> `{"queue":{"add":[{"search":{...}}]}}`
//! - `sonora.play` / `pause` / `next` / `previous` -> `{"playback":"..."}`
//!
//! Everything else in the WebDeck reference plugin (lyrics, volume, seek,
//! links, ...) is intentionally left out.

use serde_json::{json, Map, Value};
use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};
use tiktools_plugin_sdk::prelude::*;

use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

/// Where Sonora listens on Windows: a named pipe, spoken to like a file.
#[cfg(windows)]
const WINDOWS_PIPE: &str = r"\\.\pipe\sonora.sock";

/// Sonora refuses any other protocol version.
const PROTOCOL_VERSION: u32 = 2;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_REPLY_BYTES: usize = 1024 * 1024;
/// Kept below the host's default 30 s action timeout so the plugin reports a
/// clear error instead of being killed mid-request.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

/// Optional override for runs where the variable reaches this process. Under
/// the TikTools host a process plugin gets a cleared environment, so the
/// `socketPath` setting (read from the plugin data dir) is the working path.
const SOCKET_ENV: &str = "SONORA_SOCKET";
const KINDS: [&str; 4] = ["song", "album", "artist", "playlist"];

#[derive(Default)]
struct SonoraPlugin;

impl Plugin for SonoraPlugin {
    fn action(&mut self, context: &PluginContext, call: ActionCall) -> PluginResult<ActionResult> {
        let action_type = call.action_type().unwrap_or_default();
        // Process plugins receive the raw config alongside the event: the host
        // only renders templated fields for its own actions, so the field
        // declared `template: true` in plugin.json has to be resolved here.
        let mut config = Value::Object(call.config().clone());
        render_templates(&mut config, &call.event);
        strip_command_prefix(config.as_object_mut().expect("config object"));
        let request = request_for(action_type, config.as_object().expect("config object"))?;
        let socket = resolve_socket_path(context)?;
        let value = call_sonora(&request, &socket)?;

        let mut result = ActionResult::summary(summary_for(action_type));
        if let Some(value) = value {
            let text = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
            result = result.log(format!("Sonora: {text}"));
        }
        Ok(result)
    }
}

fn summary_for(action_type: &str) -> String {
    match action_type {
        "sonora.search-play" => "Sonora: searching and playing the best match".into(),
        "sonora.search-add" => "Sonora: searching and adding the best match to the queue".into(),
        "sonora.play" => "Sonora: playback resumed".into(),
        "sonora.pause" => "Sonora: playback paused".into(),
        "sonora.next" => "Sonora: skipped to the next track".into(),
        "sonora.previous" => "Sonora: returned to the previous track".into(),
        other => format!("Sonora: {other}"),
    }
}

/// Build the one request line Sonora expects for an action. Pure so it can be
/// unit-tested without a running Sonora.
fn request_for(action_type: &str, config: &Map<String, Value>) -> PluginResult<Value> {
    match action_type {
        "sonora.play" => Ok(json!({"playback": "play"})),
        "sonora.pause" => Ok(json!({"playback": "pause"})),
        "sonora.next" => Ok(json!({"playback": "next"})),
        "sonora.previous" => Ok(json!({"playback": "previous"})),
        "sonora.search-play" | "sonora.search-add" => {
            let text = required_text(config, "text")?;
            let kind = required_choice(config, "kind", &KINDS)?;
            let item = json!({ "search": { "text": text, "kind": kind } });
            let queue = match action_type {
                "sonora.search-play" => json!({ "play": [item] }),
                _ => json!({ "add": [item] }),
            };
            Ok(json!({ "queue": queue }))
        }
        other => Err(PluginError::unsupported(other)),
    }
}

fn required_text(config: &Map<String, Value>, key: &str) -> PluginResult<String> {
    let value = config
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::invalid_request(format!("{key} must be a string")))?;
    let value = value.trim();
    if value.is_empty() || value.len() > 200 {
        return Err(PluginError::invalid_request(format!(
            "{key} must have 1 to 200 characters"
        )));
    }
    Ok(value.to_owned())
}

fn required_choice(
    config: &Map<String, Value>,
    key: &str,
    allowed: &[&str],
) -> PluginResult<String> {
    let value = config
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::invalid_request(format!("{key} must be a string")))?;
    if !allowed.contains(&value) {
        return Err(PluginError::invalid_request(format!(
            "{key} must be one of: {}",
            allowed.join(", ")
        )));
    }
    Ok(value.to_owned())
}

/// Maximum length of a rendered template, matching the host's own template
/// budget closely enough while staying far below the action timeout.
const MAX_RENDERED_BYTES: usize = 8 * 1024;

/// Renders `{{ event.* }}` placeholders in every config string. Fields without
/// placeholders are untouched, so static values (the `kind` select, socket
/// paths) keep their exact text. Unknown paths render as empty strings, like
/// the host renderer.
fn render_templates(value: &mut Value, event: &Value) {
    match value {
        Value::String(text) => {
            if text.contains("{{") {
                *text = render_string(text, event);
            }
        }
        Value::Array(items) => {
            for item in items {
                render_templates(item, event);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                render_templates(item, event);
            }
        }
        _ => {}
    }
}

fn render_string(source: &str, event: &Value) -> String {
    let mut output = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(start) = rest.find("{{") {
        push_bounded(&mut output, &rest[..start]);
        let expression = &rest[start + 2..];
        let Some(end) = expression.find("}}") else {
            // Unclosed placeholder: keep the remainder literal, once.
            push_bounded(&mut output, &rest[start..]);
            return output;
        };
        let path = expression[..end].trim();
        if let Some(value) = read_event_path(event, path) {
            match value {
                Value::Null => {}
                Value::String(text) => push_bounded(&mut output, text),
                other => push_bounded(&mut output, &other.to_string()),
            }
        }
        rest = &expression[end + 2..];
    }
    push_bounded(&mut output, rest);
    output
}

fn push_bounded(output: &mut String, text: &str) {
    let remaining = MAX_RENDERED_BYTES.saturating_sub(output.len());
    if remaining == 0 {
        return;
    }
    if text.len() <= remaining {
        output.push_str(text);
        return;
    }
    let mut end = remaining.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&text[..end]);
}

/// Reads an `event.*` path from the automation envelope; a leading `event`
/// segment is optional, mirroring the host's filter/template path language.
fn read_event_path<'a>(event: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.trim();
    let path = path.strip_prefix("event.").unwrap_or(path);
    let path = path.strip_prefix("event").unwrap_or(path);
    let mut current = event;
    for part in path.trim_matches('.').split('.') {
        if part.is_empty() {
            continue;
        }
        current = current.get(part)?;
    }
    Some(current)
}

/// Removes the chat command from a rendered search text. Automations match a
/// comment with `starts with !play` — or any other `!key` — so searching the
/// literal comment would ask Sonora for the command itself and could play a
/// track named after it. An explicit `stripPrefix` wins; otherwise any
/// leading `!<word>` token is removed, so `!randomkey`, `!sonora` and `!number`
/// all work without extra configuration.
fn strip_command_prefix(config: &mut Map<String, Value>) {
    let prefix = config
        .get("stripPrefix")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let Some(Value::String(text)) = config.get_mut("text") else {
        return;
    };
    let trimmed = text.trim();
    *text = if prefix.is_empty() {
        strip_bang_token(trimmed)
    } else {
        strip_exact_prefix(trimmed, &prefix)
    }
    .to_owned();
}

/// Drops a leading `!` plus an ASCII word (`!play`, `!sonora`, `!randomkey`).
fn strip_bang_token(text: &str) -> &str {
    let Some(rest) = text.strip_prefix('!') else {
        return text;
    };
    let word_len = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
        .count();
    if word_len == 0 || word_len == rest.len() {
        return text;
    }
    rest[word_len..].trim_start()
}

/// Case-insensitive prefix removal that respects UTF-8 character boundaries.
fn strip_exact_prefix<'a>(text: &'a str, prefix: &str) -> &'a str {
    let mut end = 0usize;
    for (wanted, actual) in prefix.chars().zip(text.chars()) {
        if !wanted.eq_ignore_ascii_case(&actual) {
            return text;
        }
        end += actual.len_utf8();
    }
    if prefix.chars().count() > text.chars().count() {
        return text;
    }
    text.get(end..).unwrap_or(text).trim_start()
}

/// Resolve the socket in order: the TikTools plugin setting, an environment
/// override (for direct/standalone runs), then the standard XDG locations.
fn resolve_socket_path(context: &PluginContext) -> PluginResult<PathBuf> {
    if let Some(path) = setting_socket_path(context) {
        return Ok(path);
    }
    if let Some(path) = non_empty(env::var_os(SOCKET_ENV)) {
        return Ok(PathBuf::from(path));
    }
    socket_path_from(
        env::var_os("XDG_RUNTIME_DIR"),
        env::var_os("XDG_CACHE_HOME"),
        env::var_os("HOME"),
    )
}

/// The host writes plugin settings to `<data dir>/<plugin id>/settings.json`.
/// Process plugins have no SDK accessor for settings, but the data dir is
/// passed explicitly, so this is a stable way to honour the `socketPath`
/// setting configured in the TikTools UI.
fn setting_socket_path(context: &PluginContext) -> Option<PathBuf> {
    let root = tiktools_plugin_sdk::data_dir().ok()?;
    let id = if context.identity.id.is_empty() {
        "sonora"
    } else {
        &context.identity.id
    };
    let text = std::fs::read_to_string(root.join(id).join("settings.json")).ok()?;
    let settings: Value = serde_json::from_str(&text).ok()?;
    socket_path_from_setting(&settings)
}

fn socket_path_from_setting(settings: &Value) -> Option<PathBuf> {
    let value = settings.get("socketPath")?.as_str()?.trim();
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn socket_path_from(
    runtime_dir: Option<OsString>,
    cache_home: Option<OsString>,
    home: Option<OsString>,
) -> PluginResult<PathBuf> {
    #[cfg(unix)]
    {
        if let Some(dir) = non_empty(runtime_dir) {
            return Ok(Path::new(&dir).join("sonora.sock"));
        }
        let cache = non_empty(cache_home)
            .map(PathBuf::from)
            .or_else(|| non_empty(home).map(|home| PathBuf::from(home).join(".cache")))
            .ok_or_else(|| {
                PluginError::other("Cannot locate Sonora's socket; set SONORA_SOCKET to its path")
            })?;
        Ok(cache.join("sonora").join("sonora.sock"))
    }
    #[cfg(windows)]
    {
        let _ = (runtime_dir, cache_home, home);
        Ok(PathBuf::from(WINDOWS_PIPE))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (runtime_dir, cache_home, home);
        Err(PluginError::other(
            "Sonora control supports Linux, macOS and Windows only",
        ))
    }
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

/// Send one request and wait for a bounded reply. `Ok(None)` means Sonora
/// answered `"done"`; `Ok(Some(value))` is a value reply; a refused request
/// becomes an error the user can read in the action result.
fn call_sonora(body: &Value, path: &Path) -> PluginResult<Option<Value>> {
    #[cfg(unix)]
    {
        let request = encode_request(body)?;
        let stream = UnixStream::connect(path).map_err(|error| {
            PluginError::other(format!(
                "Cannot connect to Sonora at {} ({}). Is Sonora running?",
                path.display(),
                error
            ))
        })?;
        stream
            .set_read_timeout(Some(ANSWER_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(ANSWER_TIMEOUT)))
            .map_err(|error| {
                PluginError::other(format!("Could not set Sonora socket timeout: {error}"))
            })?;
        exchange(&stream, &request)
    }
    #[cfg(windows)]
    {
        let request = encode_request(body)?;
        let path = path.to_path_buf();
        // A named pipe opened as a file has no read timeout, so the exchange
        // runs on its own thread and the answer is awaited with a deadline. A
        // stuck thread ends with this short-lived plugin process.
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|error| {
                    PluginError::other(format!(
                        "Cannot connect to Sonora at {} ({}). Is Sonora running?",
                        path.display(),
                        error
                    ))
                })
                .and_then(|pipe| exchange(pipe, &request));
            let _ = sender.send(outcome);
        });
        receiver
            .recv_timeout(ANSWER_TIMEOUT)
            .map_err(|_| PluginError::other("Sonora did not answer in time"))?
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (body, path);
        Err(PluginError::other(
            "Sonora control supports Linux, macOS and Windows only",
        ))
    }
}

/// Write one request line and read the bounded one-line reply, over any
/// duplex byte stream (a Unix socket or a Windows named pipe).
fn exchange<S: Read + Write>(mut stream: S, request: &[u8]) -> PluginResult<Option<Value>> {
    stream.write_all(request).map_err(|error| {
        PluginError::other(format!("Could not send request to Sonora: {error}"))
    })?;
    stream.flush().map_err(|error| {
        PluginError::other(format!("Could not send request to Sonora: {error}"))
    })?;

    let mut reader = BufReader::new(stream.take(MAX_REPLY_BYTES as u64 + 1));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).map_err(|error| {
        PluginError::other(format!("Could not read Sonora's reply: {error}"))
    })?;
    if line.len() > MAX_REPLY_BYTES {
        return Err(PluginError::other("Sonora's reply is too large"));
    }
    if !line.ends_with(b"\n") {
        return Err(PluginError::other(
            "Sonora closed the connection without answering",
        ));
    }
    decode_reply(&line)
}

fn encode_request(body: &Value) -> PluginResult<Vec<u8>> {
    // A plugin must not open a long-lived streaming connection or make Sonora
    // play arbitrary local files.
    if body
        .as_object()
        .is_some_and(|map| map.contains_key("watch"))
    {
        return Err(PluginError::invalid_request(
            "streaming requests are not available to plugins",
        ));
    }
    if let Some(queue) = body.get("queue").and_then(Value::as_object) {
        let has_path = queue
            .values()
            .filter_map(Value::as_array)
            .flatten()
            .any(|item| item.get("path").is_some());
        if has_path {
            return Err(PluginError::invalid_request(
                "local file paths cannot be queued by a plugin",
            ));
        }
    }
    // Build the envelope explicitly so `v` comes first, exactly as in
    // Sonora's CLI wire format (serde_json's map would reorder the keys).
    let payload =
        serde_json::to_vec(body).map_err(|error| PluginError::other(error.to_string()))?;
    let prefix = format!(r#"{{"v":{PROTOCOL_VERSION},"body":"#);
    let mut line = Vec::with_capacity(prefix.len() + payload.len() + 2);
    line.extend_from_slice(prefix.as_bytes());
    line.extend_from_slice(&payload);
    line.extend_from_slice(b"}\n");
    if line.len() > MAX_REQUEST_BYTES {
        return Err(PluginError::invalid_request("request is too large"));
    }
    Ok(line)
}

fn decode_reply(line: &[u8]) -> PluginResult<Option<Value>> {
    let envelope: Value = serde_json::from_slice(line)
        .map_err(|error| PluginError::other(format!("Invalid reply from Sonora: {error}")))?;
    let version = envelope
        .get("v")
        .and_then(Value::as_u64)
        .ok_or_else(|| PluginError::other("Invalid reply from Sonora: missing version"))?;
    if version != u64::from(PROTOCOL_VERSION) {
        return Err(PluginError::other(format!(
            "Sonora speaks protocol {version}, this plugin expects {PROTOCOL_VERSION}"
        )));
    }
    match envelope.get("body") {
        Some(Value::String(word)) if word == "done" => Ok(None),
        Some(Value::Object(body)) => {
            if let Some(value) = body.get("value") {
                Ok(Some(value.clone()))
            } else if let Some(failure) = body.get("failed") {
                Err(PluginError::other(describe_failure(failure)))
            } else {
                Err(PluginError::other("Invalid reply from Sonora"))
            }
        }
        _ => Err(PluginError::other("Invalid reply from Sonora")),
    }
}

fn describe_failure(failure: &Value) -> String {
    match failure {
        Value::String(word) => match word.as_str() {
            "malformed" => "Sonora rejected the request as malformed".into(),
            "no-current-track" => "No track is playing in Sonora".into(),
            other => format!("Sonora refused the request ({other})"),
        },
        Value::Object(map) => {
            if let Some(version) = map.get("version") {
                format!("Sonora speaks protocol {version}, this plugin expects {PROTOCOL_VERSION}")
            } else {
                "Sonora refused the request".into()
            }
        }
        _ => "Sonora refused the request".into(),
    }
}

tiktools_process_plugin!(SonoraPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    fn config(entries: &[(&str, &str)]) -> Map<String, Value> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), Value::String((*value).to_owned())))
            .collect()
    }

    #[test]
    fn playback_actions_map_to_control_requests() {
        for (action, playback) in [
            ("sonora.play", "play"),
            ("sonora.pause", "pause"),
            ("sonora.next", "next"),
            ("sonora.previous", "previous"),
        ] {
            assert_eq!(
                request_for(action, &config(&[])).unwrap(),
                json!({"playback": playback})
            );
        }
    }

    #[test]
    fn search_play_maps_to_a_queue_play_request() {
        let body = request_for(
            "sonora.search-play",
            &config(&[("text", "daft punk"), ("kind", "song")]),
        )
        .unwrap();
        assert_eq!(
            body,
            json!({"queue": {"play": [{"search": {"text": "daft punk", "kind": "song"}}]}})
        );
    }

    #[test]
    fn search_add_maps_to_a_queue_add_request() {
        let body = request_for(
            "sonora.search-add",
            &config(&[("text", "daft punk"), ("kind", "song")]),
        )
        .unwrap();
        assert_eq!(
            body,
            json!({"queue": {"add": [{"search": {"text": "daft punk", "kind": "song"}}]}})
        );
    }

    #[test]
    fn search_play_renders_event_placeholders_before_building_the_request() {
        let mut config = Value::Object(config(&[
            ("text", "{{ event.data.comment }} "),
            ("kind", "song"),
        ]));
        let event = json!({
            "id": "sample-event",
            "type": "tiktok.chat",
            "data": {"comment": "!play vaundy", "method": "WebcastChatMessage"},
            "user": {"uniqueId": "usuario_demo"}
        });
        render_templates(&mut config, &event);
        strip_command_prefix(config.as_object_mut().unwrap());
        assert_eq!(
            request_for("sonora.search-play", config.as_object().unwrap()).unwrap(),
            json!({"queue": {"play": [{"search": {"text": "vaundy", "kind": "song"}}]}})
        );
    }

    #[test]
    fn strips_any_bang_command_not_only_play() {
        for (comment, expected) in [
            ("!randomkey Daft Punk", "Daft Punk"),
            ("!sonora   Beethoven", "Beethoven"),
            ("!number 42", "42"),
            ("!play vaundy", "vaundy"),
            ("Daft Punk", "Daft Punk"),
        ] {
            let mut config = config(&[("text", comment), ("kind", "song")]);
            strip_command_prefix(&mut config);
            assert_eq!(config["text"], expected, "comment: {comment}");
        }
    }

    #[test]
    fn explicit_strip_prefix_overrides_the_bang_default() {
        let mut play_config = config(&[
            ("text", "PLAY Daft Punk"),
            ("stripPrefix", "play"),
            ("kind", "song"),
        ]);
        strip_command_prefix(&mut play_config);
        assert_eq!(play_config["text"], "Daft Punk");

        // A prefix that is not present leaves the text alone.
        let mut other_config = config(&[
            ("text", "!play vaundy"),
            ("stripPrefix", "sonora"),
            ("kind", "song"),
        ]);
        strip_command_prefix(&mut other_config);
        assert_eq!(other_config["text"], "!play vaundy");
    }

    #[test]
    fn templates_resolve_identity_paths_and_render_unknown_paths_empty() {
        let event = json!({
            "type": "tiktok.chat",
            "data": {"comment": "hola"},
            "user": {"uniqueId": "luna_dev"}
        });
        let mut config = json!({
            "message": "{{event.user.uniqueId}}: {{ event.data.comment }} / {{ event.data.missing }} ({{ event.type }})"
        });
        render_templates(&mut config, &event);
        assert_eq!(config["message"], "luna_dev: hola /  (tiktok.chat)");
    }

    #[test]
    fn templates_leave_unclosed_placeholders_literal() {
        let event = json!({"data": {"comment": "hola"}});
        let mut config = json!({"text": "{{ event.data.comment }} plus {{ broken"});
        render_templates(&mut config, &event);
        assert_eq!(config["text"], "hola plus {{ broken");
    }

    #[test]
    fn search_actions_validate_text_and_kind() {
        for action in ["sonora.search-play", "sonora.search-add"] {
            assert!(request_for(action, &config(&[("kind", "song")])).is_err());
            assert!(request_for(action, &config(&[("text", "x"), ("kind", "movie")])).is_err());
            assert!(request_for(action, &config(&[("text", "   "), ("kind", "song")])).is_err());
        }
    }

    #[test]
    fn unknown_actions_are_rejected() {
        assert!(request_for("sonora.seek", &config(&[])).is_err());
    }

    #[test]
    fn request_envelope_uses_protocol_two() {
        assert_eq!(
            encode_request(&json!({"playback": "next"})).unwrap(),
            b"{\"v\":2,\"body\":{\"playback\":\"next\"}}\n"
        );
    }

    #[test]
    fn replies_decode_done_value_and_failure() {
        assert_eq!(
            decode_reply(b"{\"v\":2,\"body\":\"done\"}\n").unwrap(),
            None
        );
        assert_eq!(
            decode_reply(b"{\"v\":2,\"body\":{\"value\":{\"hits\":[]}}}\n").unwrap(),
            Some(json!({"hits": []}))
        );
        assert!(decode_reply(b"{\"v\":2,\"body\":{\"failed\":\"no-current-track\"}}\n").is_err());
        assert!(decode_reply(b"{\"v\":3,\"body\":\"done\"}\n").is_err());
    }

    #[test]
    fn socket_path_setting_when_present() {
        let settings = json!({"socketPath": "  /tmp/custom.sock  "});
        assert_eq!(
            socket_path_from_setting(&settings),
            Some(PathBuf::from("/tmp/custom.sock"))
        );
        assert_eq!(socket_path_from_setting(&json!({"socketPath": ""})), None);
        assert_eq!(socket_path_from_setting(&json!({})), None);
    }

    /// In-memory duplex stream: reads come from `reply`, writes land in `sent`.
    struct Duplex {
        reply: std::io::Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reply.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.sent.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn exchange_sends_one_line_and_decodes_the_reply() {
        let request = encode_request(&json!({"playback": "pause"})).unwrap();
        let mut stream = Duplex {
            reply: std::io::Cursor::new(b"{\"v\":2,\"body\":\"done\"}\n".to_vec()),
            sent: Vec::new(),
        };
        assert_eq!(exchange(&mut stream, &request).unwrap(), None);
        assert_eq!(stream.sent, request);
    }

    #[test]
    fn exchange_rejects_a_reply_without_a_newline() {
        let request = encode_request(&json!({"playback": "play"})).unwrap();
        let stream = Duplex {
            reply: std::io::Cursor::new(b"{\"v\":2,\"body\":\"done\"}".to_vec()),
            sent: Vec::new(),
        };
        assert!(exchange(stream, &request).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_is_the_sonora_named_pipe() {
        assert_eq!(
            socket_path_from(None, None, None).unwrap(),
            PathBuf::from(r"\\.\pipe\sonora.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_path_prefers_runtime_then_cache() {
        let runtime = socket_path_from(Some(OsString::from("/run/user/1000")), None, None).unwrap();
        assert_eq!(runtime, PathBuf::from("/run/user/1000/sonora.sock"));

        let cache = socket_path_from(None, Some(OsString::from("/home/me/.cache")), None).unwrap();
        assert_eq!(cache, PathBuf::from("/home/me/.cache/sonora/sonora.sock"));

        let home = socket_path_from(None, None, Some(OsString::from("/home/me"))).unwrap();
        assert_eq!(home, PathBuf::from("/home/me/.cache/sonora/sonora.sock"));
    }
}
