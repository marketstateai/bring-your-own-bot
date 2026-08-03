#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, State, WindowEvent};
use uuid::Uuid;

const CONNECTOR_HOST: &str = "127.0.0.1";
const CONNECTOR_PORT: u16 = 8765;
const CONNECTOR_ENDPOINT: &str = "http://127.0.0.1:8765";
const MAX_HTTP_BODY_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_TIMEOUT_SECONDS: u64 = 900;
const AUTOSTART_ID: &str = "ai.marketstate.codex-connector";
const MARKETSTATE_SUPABASE_URL: &str = "https://rlvpjjbaommdeyzmwqqd.supabase.co";
const MARKETSTATE_SUPABASE_PUBLISHABLE_KEY: &str = "sb_publishable_xSXpGiFapUycMFTZzlPBzA_OfcSdvg5";
const OWNER_FILE_NAME: &str = "owner.json";
const AUTH_CACHE_SECONDS: u64 = 30;

#[derive(Clone)]
struct ActiveRequest {
    child: Arc<Mutex<Child>>,
    cancelled: Arc<AtomicBool>,
}

type ActiveRequests = Arc<Mutex<HashMap<String, ActiveRequest>>>;

struct BridgeService {
    stop: Option<Arc<AtomicBool>>,
    thread: Option<thread::JoinHandle<()>>,
    active_requests: ActiveRequests,
    externally_managed: bool,
    last_error: String,
}

impl BridgeService {
    fn new() -> Self {
        Self {
            stop: None,
            thread: None,
            active_requests: Arc::new(Mutex::new(HashMap::new())),
            externally_managed: false,
            last_error: String::new(),
        }
    }

    fn start(
        &mut self,
        codex_bin: PathBuf,
        workspace: PathBuf,
        owner: Arc<Mutex<OwnerState>>,
    ) -> Result<(), String> {
        self.stop();
        self.last_error.clear();
        self.externally_managed = false;

        if endpoint_is_open() {
            self.externally_managed = true;
            return Ok(());
        }

        let listener = TcpListener::bind((CONNECTOR_HOST, CONNECTOR_PORT))
            .map_err(|error| format!("Unable to start the connector: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("Unable to configure the connector: {error}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let active_requests = Arc::clone(&self.active_requests);
        let handle = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        if !peer.ip().is_loopback() {
                            continue;
                        }
                        let codex = codex_bin.clone();
                        let workspace = workspace.clone();
                        let active = Arc::clone(&active_requests);
                        let owner = Arc::clone(&owner);
                        thread::spawn(move || {
                            handle_http_connection(stream, codex, workspace, active, owner)
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(40));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        });

        self.stop = Some(stop);
        self.thread = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = TcpStream::connect((CONNECTOR_HOST, CONNECTOR_PORT));
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        if !self.externally_managed {
            cancel_all_requests(&self.active_requests);
        }
        self.externally_managed = false;
    }

    fn is_running(&self) -> bool {
        endpoint_is_open()
    }
}

impl Drop for BridgeService {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct LoginProgress {
    in_progress: bool,
    message: String,
}

struct ConnectorState {
    bridge: Mutex<BridgeService>,
    codex_bin: PathBuf,
    workspace: PathBuf,
    config_dir: PathBuf,
    login: Arc<Mutex<LoginProgress>>,
    owner: Arc<Mutex<OwnerState>>,
}

#[derive(Clone, Deserialize, Serialize)]
struct OwnerBinding {
    user_id: String,
    email: String,
}

struct CachedIdentity {
    access_token: String,
    identity: OwnerBinding,
    checked_at: Instant,
}

struct OwnerState {
    binding: Option<OwnerBinding>,
    owner_path: PathBuf,
    auth_cache: Option<CachedIdentity>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorStatus {
    service_running: bool,
    externally_managed: bool,
    endpoint: &'static str,
    codex_found: bool,
    codex_version: String,
    authenticated: bool,
    auth_status: String,
    launch_at_login: bool,
    login_in_progress: bool,
    last_error: String,
    owner_bound: bool,
    owner_email: String,
}

#[derive(Default)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    origin: Option<String>,
}

impl HttpResponse {
    fn json(status: u16, reason: &'static str, payload: Value, origin: Option<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(&payload)
                .unwrap_or_else(|_| b"{\"error\":\"Serialization failed.\"}".to_vec()),
            origin,
        }
    }

    fn text(status: u16, reason: &'static str, body: &str, origin: Option<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
            origin,
        }
    }

    fn empty(status: u16, reason: &'static str, origin: Option<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
            origin,
        }
    }
}

fn endpoint_is_open() -> bool {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), CONNECTOR_PORT);
    TcpStream::connect_timeout(&address, Duration::from_millis(120)).is_ok()
}

fn find_codex_binary() -> Option<PathBuf> {
    if let Some(path) = env::var_os("CODEX_BIN").map(PathBuf::from) {
        if executable_exists(&path) {
            return Some(path);
        }
    }

    if let Ok(executable) = env::current_exe() {
        if let Some(parent) = executable.parent() {
            for name in ["codex", "codex.exe"] {
                let candidate = parent.join(name);
                if executable_exists(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }

    if let Some(paths) = env::var_os("PATH") {
        for directory in env::split_paths(&paths) {
            for name in ["codex", "codex.exe"] {
                let candidate = directory.join(name);
                if executable_exists(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }

    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ];
    if let Some(home) = home_dir() {
        candidates.extend([
            home.join(".local/bin/codex"),
            home.join(".codex/bin/codex"),
            home.join(".codex/bin/codex.exe"),
            home.join(".codex/current/bin/codex"),
            home.join(".codex/current/bin/codex.exe"),
        ]);
    }
    candidates
        .into_iter()
        .find(|candidate| executable_exists(candidate))
}

fn executable_exists(path: &Path) -> bool {
    path.is_file()
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn command_output(command: &Path, arguments: &[&str]) -> Result<(bool, String), String> {
    let output = Command::new(command)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stdout.is_empty() { stderr } else { stdout };
    Ok((output.status.success(), detail))
}

fn codex_version(codex_bin: &Path) -> String {
    command_output(codex_bin, &["--version"])
        .map(|(_, output)| output)
        .unwrap_or_default()
}

fn codex_auth_status(codex_bin: &Path) -> (bool, String) {
    match command_output(codex_bin, &["login", "status"]) {
        Ok((true, output)) => (
            true,
            if output.is_empty() {
                "Connected to Codex".into()
            } else {
                output
            },
        ),
        Ok((false, output)) => (
            false,
            if output.is_empty() {
                "Sign in to your Codex account".into()
            } else {
                output
            },
        ),
        Err(error) => (
            false,
            format!("Unable to check Codex authentication: {error}"),
        ),
    }
}

fn load_owner_binding(path: &Path) -> Option<OwnerBinding> {
    let bytes = fs::read(path).ok()?;
    let binding = serde_json::from_slice::<OwnerBinding>(&bytes).ok()?;
    (!binding.user_id.trim().is_empty()).then_some(binding)
}

fn save_owner_binding(path: &Path, binding: &OwnerBinding) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("Owner configuration path is invalid.")?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Unable to create connector configuration: {error}"))?;
    let temporary = parent.join(format!(".{OWNER_FILE_NAME}.{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(binding).map_err(|error| error.to_string())?;
    fs::write(&temporary, bytes)
        .map_err(|error| format!("Unable to save connector owner: {error}"))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("Unable to activate connector owner: {error}"))
}

fn bearer_token(headers: &HashMap<String, String>) -> Result<&str, String> {
    let value = headers
        .get("authorization")
        .ok_or("Sign in to MarketState before using the Codex connector.")?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or("The MarketState authorization header is invalid.")?;
    if token.len() > 16_384 || token.chars().any(|character| character.is_control()) {
        return Err("The MarketState access token is invalid.".into());
    }
    Ok(token)
}

fn validate_marketstate_token(access_token: &str) -> Result<OwnerBinding, String> {
    let mut child = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--max-time",
            "10",
            "--header",
            &format!("apikey: {MARKETSTATE_SUPABASE_PUBLISHABLE_KEY}"),
            "--config",
            "-",
            &format!("{MARKETSTATE_SUPABASE_URL}/auth/v1/user"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Unable to validate the MarketState account: {error}"))?;

    let escaped_token = access_token.replace('\\', "\\\\").replace('"', "\\\"");
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(format!("header = \"Authorization: Bearer {escaped_token}\"\n").as_bytes())
            .map_err(|error| format!("Unable to validate the MarketState account: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("Unable to validate the MarketState account: {error}"))?;
    if !output.status.success() {
        return Err(
            "Your MarketState session is invalid or expired. Sign in again and retry.".into(),
        );
    }
    let payload = serde_json::from_slice::<Value>(&output.stdout)
        .map_err(|_| "MarketState returned an invalid account response.".to_string())?;
    let user_id = payload
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("MarketState did not return a user identity.")?
        .to_string();
    let email = payload
        .get("email")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    Ok(OwnerBinding { user_id, email })
}

fn authorize_marketstate_user(
    headers: &HashMap<String, String>,
    owner_state: &Arc<Mutex<OwnerState>>,
) -> Result<OwnerBinding, (u16, &'static str, &'static str, String)> {
    let access_token = bearer_token(headers)
        .map_err(|message| (401, "Unauthorized", "marketstate_login_required", message))?;

    let cached = {
        let owner = owner_state.lock().map_err(|_| {
            (
                500,
                "Internal Server Error",
                "owner_state_unavailable",
                "Connector account state is unavailable.".into(),
            )
        })?;
        owner.auth_cache.as_ref().and_then(|cached| {
            (cached.access_token == access_token
                && cached.checked_at.elapsed() < Duration::from_secs(AUTH_CACHE_SECONDS))
            .then(|| cached.identity.clone())
        })
    };
    let identity = match cached {
        Some(identity) => identity,
        None => validate_marketstate_token(access_token)
            .map_err(|message| (401, "Unauthorized", "marketstate_session_invalid", message))?,
    };

    let mut owner = owner_state.lock().map_err(|_| {
        (
            500,
            "Internal Server Error",
            "owner_state_unavailable",
            "Connector account state is unavailable.".into(),
        )
    })?;
    if let Some(binding) = &owner.binding {
        if binding.user_id != identity.user_id {
            return Err((
                403,
                "Forbidden",
                "connector_owned_by_another_user",
                format!(
                    "This connector belongs to {}. Open the connector and explicitly unlink that account before changing users.",
                    if binding.email.is_empty() { "another MarketState user" } else { &binding.email }
                ),
            ));
        }
    } else {
        save_owner_binding(&owner.owner_path, &identity).map_err(|message| {
            (
                500,
                "Internal Server Error",
                "owner_binding_failed",
                message,
            )
        })?;
        owner.binding = Some(identity.clone());
    }
    owner.auth_cache = Some(CachedIdentity {
        access_token: access_token.to_string(),
        identity: identity.clone(),
        checked_at: Instant::now(),
    });
    Ok(identity)
}

fn allowed_origin(origin: Option<&str>) -> Option<String> {
    let origin = origin?.trim();
    let local = matches!(
        origin,
        "http://127.0.0.1:5174"
            | "http://localhost:5174"
            | "tauri://localhost"
            | "https://tauri.localhost"
    );
    let marketstate = origin == "https://marketstate.ai"
        || origin == "https://www.marketstate.ai"
        || origin
            .strip_prefix("https://")
            .is_some_and(|host| host.ends_with(".marketstate.ai") && !host.contains('/'));
    (local || marketstate).then(|| origin.to_string())
}

fn valid_host(value: Option<&String>) -> bool {
    value.is_some_and(|host| {
        let host = host.to_ascii_lowercase();
        host == "127.0.0.1:8765" || host == "localhost:8765" || host == "[::1]:8765"
    })
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(8192);
    let mut chunk = [0_u8; 4096];
    let header_end;

    loop {
        let count = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("Connection closed before the request was complete.".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > 64 * 1024 {
            return Err("HTTP headers are too large.".into());
        }
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
    }

    let header_text = String::from_utf8(bytes[..header_end].to_vec())
        .map_err(|_| "HTTP headers must be UTF-8.".to_string())?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().ok_or("Missing HTTP request line.")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let path = request_parts.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        return Err("Invalid HTTP request line.".into());
    }

    let mut headers = HashMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|_| "Invalid Content-Length.".to_string())?
        .unwrap_or(0);
    if content_length > MAX_HTTP_BODY_BYTES {
        return Err("Request body is too large.".into());
    }

    let mut body = bytes[header_end..].to_vec();
    while body.len() < content_length {
        let count = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("Connection closed before the body was complete.".into());
        }
        body.extend_from_slice(&chunk[..count]);
    }
    body.truncate(content_length);

    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn write_http_response(stream: &mut TcpStream, response: HttpResponse) {
    let mut headers = format!(
    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nAccess-Control-Allow-Private-Network: true\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\n",
    response.status,
    response.reason,
    response.content_type,
    response.body.len()
  );
    if let Some(origin) = response.origin {
        headers.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"
        ));
    }
    headers.push_str("\r\n");
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

fn handle_http_connection(
    mut stream: TcpStream,
    codex_bin: PathBuf,
    workspace: PathBuf,
    active_requests: ActiveRequests,
    owner_state: Arc<Mutex<OwnerState>>,
) {
    let request = match read_http_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            write_http_response(
                &mut stream,
                HttpResponse::json(400, "Bad Request", json!({ "error": error }), None),
            );
            return;
        }
    };

    if !valid_host(request.headers.get("host")) {
        write_http_response(
            &mut stream,
            HttpResponse::json(
                403,
                "Forbidden",
                json!({ "error": "Invalid loopback host." }),
                None,
            ),
        );
        return;
    }

    let request_origin = request.headers.get("origin").map(String::as_str);
    let origin = allowed_origin(request_origin);
    if request_origin.is_some() && origin.is_none() {
        write_http_response(
            &mut stream,
            HttpResponse::json(
                403,
                "Forbidden",
                json!({ "error": "Origin is not allowed." }),
                None,
            ),
        );
        return;
    }

    let requires_user = matches!(request.path.as_str(), "/health" | "/chat" | "/cancel")
        && request.method != "OPTIONS";
    let authorized_user = if requires_user {
        match authorize_marketstate_user(&request.headers, &owner_state) {
            Ok(identity) => Some(identity),
            Err((status, reason, code, error)) => {
                write_http_response(
                    &mut stream,
                    HttpResponse::json(
                        status,
                        reason,
                        json!({ "ok": false, "code": code, "error": error }),
                        origin,
                    ),
                );
                return;
            }
        }
    } else {
        None
    };

    let response = match (request.method.as_str(), request.path.as_str()) {
        ("OPTIONS", _) => HttpResponse::empty(204, "No Content", origin),
        ("GET", "/") => HttpResponse::text(
            200,
            "OK",
            "MarketState Codex Connector is running.\n",
            origin,
        ),
        ("GET", "/health") => {
            let (authenticated, auth_status) = codex_auth_status(&codex_bin);
            let active_count = active_requests.lock().map(|items| items.len()).unwrap_or(0);
            HttpResponse::json(
                200,
                "OK",
                json!({
                  "ok": true,
                  "authenticated": authenticated,
                  "auth_status": auth_status,
                  "codex_bin": codex_bin,
                  "codex_version": codex_version(&codex_bin),
                  "workspace_root": workspace,
                  "auth_required": true,
                  "marketstate_user": authorized_user,
                  "active_requests": active_count,
                  "managed_by": "marketstate-codex-connector"
                }),
                origin,
            )
        }
        ("POST", "/cancel") => {
            let payload =
                serde_json::from_slice::<Value>(&request.body).unwrap_or_else(|_| json!({}));
            let request_id = payload
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match cancel_request(&active_requests, request_id) {
                true => HttpResponse::json(
                    200,
                    "OK",
                    json!({ "ok": true, "request_id": request_id, "cancelled": true }),
                    origin,
                ),
                false => HttpResponse::json(
                    404,
                    "Not Found",
                    json!({ "ok": false, "request_id": request_id, "cancelled": false, "error": "No active request has that request_id." }),
                    origin,
                ),
            }
        }
        ("POST", "/chat") => match serde_json::from_slice::<Value>(&request.body) {
            Ok(payload) => {
                match run_codex_request(payload, &codex_bin, &workspace, &active_requests) {
                    Ok(result) => {
                        let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
                        HttpResponse::json(
                            if ok { 200 } else { 502 },
                            if ok { "OK" } else { "Bad Gateway" },
                            result,
                            origin,
                        )
                    }
                    Err(error) => {
                        HttpResponse::json(400, "Bad Request", json!({ "error": error }), origin)
                    }
                }
            }
            Err(_) => HttpResponse::json(
                400,
                "Bad Request",
                json!({ "error": "Invalid JSON body." }),
                origin,
            ),
        },
        _ => HttpResponse::json(404, "Not Found", json!({ "error": "Not found." }), origin),
    };

    write_http_response(&mut stream, response);
}

fn text_field<'a>(payload: &'a Value, name: &str) -> Option<&'a str> {
    payload
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn safe_request_id(payload: &Value) -> Result<String, String> {
    let request_id = text_field(payload, "request_id")
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let valid = request_id.len() <= 128
        && request_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._:-".contains(character));
    if !valid {
        return Err("request_id contains unsupported characters.".into());
    }
    Ok(request_id)
}

fn run_codex_request(
    payload: Value,
    codex_bin: &Path,
    workspace: &Path,
    active_requests: &ActiveRequests,
) -> Result<Value, String> {
    let message = text_field(&payload, "message")
        .ok_or("message is required.")?
        .to_string();
    let request_id = safe_request_id(&payload)?;
    let timeout_seconds = payload
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .clamp(1, 3600);
    let isolated = payload
        .get("isolated_workspace")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cwd = if isolated {
        env::temp_dir().join("marketstate-orama-assistant")
    } else {
        workspace.to_path_buf()
    };
    fs::create_dir_all(&cwd).map_err(|error| format!("Unable to prepare workspace: {error}"))?;

    let output_path = env::temp_dir().join(format!("codex-last-message-{}.txt", Uuid::new_v4()));
    let schema_path = payload
        .get("output_schema")
        .and_then(Value::as_object)
        .map(|schema| {
            let path = env::temp_dir().join(format!("codex-output-schema-{}.json", Uuid::new_v4()));
            (path, schema)
        });
    if let Some((path, schema)) = &schema_path {
        fs::write(
            path,
            serde_json::to_vec(schema).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("Unable to write the response schema: {error}"))?;
    }

    let mut args = Vec::<String>::new();
    if let Some(session_id) = text_field(&payload, "session_id") {
        args.extend([
            "exec".into(),
            "resume".into(),
            "--json".into(),
            "--output-last-message".into(),
        ]);
        args.push(output_path.to_string_lossy().into_owned());
        if let Some(model) = text_field(&payload, "model") {
            args.extend(["--model".into(), model.into()]);
        }
        if let Some(effort) = text_field(&payload, "reasoning_effort") {
            args.extend(["-c".into(), format!("model_reasoning_effort=\"{effort}\"")]);
        }
        args.extend([
            "--skip-git-repo-check".into(),
            session_id.into(),
            "-".into(),
        ]);
    } else {
        args.extend([
            "exec".into(),
            "--json".into(),
            "--color".into(),
            "never".into(),
            "--output-last-message".into(),
            output_path.to_string_lossy().into_owned(),
            "--cd".into(),
            cwd.to_string_lossy().into_owned(),
        ]);
        if let Some(model) = text_field(&payload, "model") {
            args.extend(["--model".into(), model.into()]);
        }
        if let Some(effort) = text_field(&payload, "reasoning_effort") {
            args.extend(["-c".into(), format!("model_reasoning_effort=\"{effort}\"")]);
        }
        let sandbox = text_field(&payload, "sandbox").unwrap_or("read-only");
        args.extend([
            "--sandbox".into(),
            sandbox.into(),
            "--skip-git-repo-check".into(),
        ]);
        if payload
            .get("ephemeral")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            args.push("--ephemeral".into());
        }
        if payload
            .get("ignore_user_config")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            args.push("--ignore-user-config".into());
        }
        if payload
            .get("ignore_rules")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            args.push("--ignore-rules".into());
        }
        if let Some((path, _)) = &schema_path {
            args.extend([
                "--output-schema".into(),
                path.to_string_lossy().into_owned(),
            ]);
        }
        args.push("-".into());
    }

    let started = Instant::now();
    let mut child = Command::new(codex_bin)
        .args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Unable to start Codex: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(message.as_bytes())
            .map_err(|error| format!("Unable to send the prompt to Codex: {error}"))?;
    }

    let mut stdout = child.stdout.take().ok_or("Codex stdout is unavailable.")?;
    let mut stderr = child.stderr.take().ok_or("Codex stderr is unavailable.")?;
    let stdout_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let stderr_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let child = Arc::new(Mutex::new(child));
    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let mut active = active_requests
            .lock()
            .map_err(|_| "Request registry is unavailable.")?;
        if active.contains_key(&request_id) {
            let _ = child.lock().map(|mut process| process.kill());
            return Err("request_id is already active.".into());
        }
        active.insert(
            request_id.clone(),
            ActiveRequest {
                child: Arc::clone(&child),
                cancelled: Arc::clone(&cancelled),
            },
        );
    }

    let mut timed_out = false;
    let exit_status = loop {
        let status = child
            .lock()
            .map_err(|_| "Codex process is unavailable.")?
            .try_wait()
            .map_err(|error| format!("Unable to read Codex status: {error}"))?;
        if let Some(status) = status {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(timeout_seconds) {
            timed_out = true;
            let _ = child.lock().map(|mut process| process.kill());
        }
        if timed_out || cancelled.load(Ordering::Relaxed) {
            let status = child
                .lock()
                .map_err(|_| "Codex process is unavailable.")?
                .wait()
                .map_err(|error| format!("Unable to stop Codex: {error}"))?;
            break status;
        }
        thread::sleep(Duration::from_millis(50));
    };

    if let Ok(mut active) = active_requests.lock() {
        active.remove(&request_id);
    }
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let was_cancelled = cancelled.load(Ordering::Relaxed);
    let mut final_message = fs::read_to_string(&output_path)
        .unwrap_or_default()
        .trim()
        .to_string();
    if timed_out || was_cancelled {
        final_message.clear();
    }

    let _ = fs::remove_file(&output_path);
    if let Some((path, _)) = schema_path {
        let _ = fs::remove_file(path);
    }

    let return_code = exit_status.code().unwrap_or(-1);
    let ok = exit_status.success() && !timed_out && !was_cancelled;
    let error = if was_cancelled {
        "Codex request was cancelled.".to_string()
    } else if timed_out {
        format!("Codex timed out after {timeout_seconds} seconds.")
    } else if !exit_status.success() {
        stderr.trim().to_string()
    } else {
        String::new()
    };
    let session_id = parse_session_id(&stdout);
    let compact = payload
        .get("compact_response")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut result = json!({
      "ok": ok,
      "returncode": return_code,
      "request_id": request_id,
      "cancelled": was_cancelled,
      "timed_out": timed_out,
      "session_id": session_id,
      "final_message": final_message,
      "elapsed_seconds": (started.elapsed().as_millis() as f64 / 1000.0),
      "error": error
    });
    if !compact {
        result["cwd"] = json!(cwd);
        result["stdout"] = json!(stdout);
        result["stderr"] = json!(stderr);
        result["events"] = json!(parse_json_lines(&stdout));
    }
    Ok(result)
}

fn parse_json_lines(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect()
}

fn parse_session_id(stdout: &str) -> Option<String> {
    for event in parse_json_lines(stdout) {
        if let Some(value) = find_string_field(
            &event,
            &["session_id", "sessionId", "thread_id", "threadId"],
        ) {
            return Some(value);
        }
    }
    None
}

fn find_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key).and_then(Value::as_str) {
                    return Some(value.to_string());
                }
            }
            object
                .values()
                .find_map(|value| find_string_field(value, keys))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|value| find_string_field(value, keys)),
        _ => None,
    }
}

fn cancel_request(active_requests: &ActiveRequests, request_id: &str) -> bool {
    let request = active_requests
        .lock()
        .ok()
        .and_then(|active| active.get(request_id).cloned());
    if let Some(request) = request {
        request.cancelled.store(true, Ordering::Relaxed);
        let _ = request.child.lock().map(|mut child| child.kill());
        true
    } else {
        false
    }
}

fn cancel_all_requests(active_requests: &ActiveRequests) {
    let requests = active_requests
        .lock()
        .map(|active| active.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    for request in requests {
        request.cancelled.store(true, Ordering::Relaxed);
        let _ = request.child.lock().map(|mut child| child.kill());
    }
}

fn preferences_path(config_dir: &Path) -> PathBuf {
    config_dir.join("preferences.json")
}

fn load_launch_preference(config_dir: &Path) -> bool {
    let path = preferences_path(config_dir);
    let Ok(contents) = fs::read_to_string(path) else {
        return true;
    };
    serde_json::from_str::<Value>(&contents)
        .ok()
        .and_then(|value| value.get("launchAtLogin").and_then(Value::as_bool))
        .unwrap_or(true)
}

fn save_launch_preference(config_dir: &Path, enabled: bool) -> Result<(), String> {
    fs::create_dir_all(config_dir).map_err(|error| error.to_string())?;
    fs::write(
        preferences_path(config_dir),
        serde_json::to_vec_pretty(&json!({ "launchAtLogin": enabled }))
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn autostart_file() -> Result<PathBuf, String> {
    Ok(home_dir()
        .ok_or("Unable to locate the home directory.")?
        .join("Library/LaunchAgents")
        .join(format!("{AUTOSTART_ID}.plist")))
}

#[cfg(target_os = "macos")]
fn set_os_autostart(enabled: bool) -> Result<(), String> {
    let path = autostart_file()?;
    if !enabled {
        if path.exists() {
            fs::remove_file(path).map_err(|error| error.to_string())?;
        }
        return Ok(());
    }
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let parent = path.parent().ok_or("Invalid LaunchAgents directory.")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let executable = xml_escape(&executable.to_string_lossy());
    let plist = format!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{AUTOSTART_ID}</string><key>ProgramArguments</key><array><string>{executable}</string><string>--background</string></array><key>RunAtLoad</key><true/></dict></plist>\n"
  );
    fs::write(path, plist).map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn os_autostart_enabled() -> bool {
    autostart_file().is_ok_and(|path| path.exists())
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "windows")]
fn set_os_autostart(enabled: bool) -> Result<(), String> {
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let status = if enabled {
        let executable = env::current_exe().map_err(|error| error.to_string())?;
        let value = format!("\"{}\" --background", executable.display());
        Command::new("reg.exe")
            .args([
                "add",
                key,
                "/v",
                "MarketStateCodexConnector",
                "/t",
                "REG_SZ",
                "/d",
                &value,
                "/f",
            ])
            .status()
    } else {
        Command::new("reg.exe")
            .args(["delete", key, "/v", "MarketStateCodexConnector", "/f"])
            .status()
    }
    .map_err(|error| error.to_string())?;
    if enabled && !status.success() {
        return Err("Windows could not enable launch at login.".into());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn os_autostart_enabled() -> bool {
    Command::new("reg.exe")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "MarketStateCodexConnector",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn set_os_autostart(_enabled: bool) -> Result<(), String> {
    Err("Launch at login is currently supported on macOS and Windows.".into())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn os_autostart_enabled() -> bool {
    false
}

#[tauri::command]
fn connector_status(state: State<'_, ConnectorState>) -> ConnectorStatus {
    let bridge = state
        .bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let login = state
        .login
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let owner = state
        .owner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let codex_found = executable_exists(&state.codex_bin);
    let (authenticated, auth_status) = if codex_found {
        codex_auth_status(&state.codex_bin)
    } else {
        (false, "Codex is not installed.".into())
    };
    ConnectorStatus {
        service_running: bridge.is_running(),
        externally_managed: bridge.externally_managed,
        endpoint: CONNECTOR_ENDPOINT,
        codex_found,
        codex_version: if codex_found {
            codex_version(&state.codex_bin)
        } else {
            String::new()
        },
        authenticated,
        auth_status: if login.in_progress && !login.message.is_empty() {
            login.message.clone()
        } else {
            auth_status
        },
        launch_at_login: os_autostart_enabled(),
        login_in_progress: login.in_progress,
        last_error: bridge.last_error.clone(),
        owner_bound: owner.binding.is_some(),
        owner_email: owner
            .binding
            .as_ref()
            .map(|binding| binding.email.clone())
            .unwrap_or_default(),
    }
}

#[tauri::command]
fn set_launch_at_login(enabled: bool, state: State<'_, ConnectorState>) -> Result<bool, String> {
    set_os_autostart(enabled)?;
    save_launch_preference(&state.config_dir, enabled)?;
    Ok(os_autostart_enabled())
}

#[tauri::command]
fn restart_connector(state: State<'_, ConnectorState>) -> Result<(), String> {
    let mut bridge = state
        .bridge
        .lock()
        .map_err(|_| "Connector state is unavailable.")?;
    if bridge.externally_managed {
        return Err(
            "Another local bridge owns port 8765. Close it before restarting this connector."
                .into(),
        );
    }
    bridge.start(
        state.codex_bin.clone(),
        state.workspace.clone(),
        Arc::clone(&state.owner),
    )
}

#[tauri::command]
fn unlink_marketstate_user(state: State<'_, ConnectorState>) -> Result<(), String> {
    if executable_exists(&state.codex_bin) {
        let (success, detail) = command_output(&state.codex_bin, &["logout"])
            .map_err(|error| format!("Unable to sign out of Codex: {error}"))?;
        if !success {
            return Err(if detail.is_empty() {
                "Codex sign-out failed, so the MarketState user was not unlinked.".into()
            } else {
                format!("Codex sign-out failed: {detail}")
            });
        }
    }
    let mut owner = state
        .owner
        .lock()
        .map_err(|_| "Connector account state is unavailable.")?;
    if owner.owner_path.exists() {
        fs::remove_file(&owner.owner_path)
            .map_err(|error| format!("Unable to unlink the MarketState account: {error}"))?;
    }
    owner.binding = None;
    owner.auth_cache = None;
    Ok(())
}

#[tauri::command]
fn begin_codex_login(state: State<'_, ConnectorState>) -> Result<(), String> {
    if !executable_exists(&state.codex_bin) {
        return Err("Codex is not installed.".into());
    }
    {
        let mut login = state
            .login
            .lock()
            .map_err(|_| "Login state is unavailable.")?;
        if login.in_progress {
            return Ok(());
        }
        login.in_progress = true;
        login.message = "Complete sign-in in the browser opened by OpenAI.".into();
    }

    let codex_bin = state.codex_bin.clone();
    let login_state = Arc::clone(&state.login);
    thread::spawn(move || {
        let result = Command::new(codex_bin)
            .arg("login")
            .stdin(Stdio::null())
            .output();
        let mut login = login_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        login.in_progress = false;
        login.message = match result {
            Ok(output) if output.status.success() => "Codex account connected.".into(),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if stderr.is_empty() {
                    "Codex sign-in did not complete.".into()
                } else {
                    stderr
                }
            }
            Err(error) => format!("Unable to start Codex sign-in: {error}"),
        };
    });
    Ok(())
}

#[tauri::command]
fn test_connection(state: State<'_, ConnectorState>) -> Result<String, String> {
    let payload = json!({
      "message": "This is a MarketState connector health check. Reply with exactly: Ready",
      "sandbox": "read-only",
      "timeout_seconds": 180,
      "ephemeral": true,
      "compact_response": true,
      "isolated_workspace": true,
      "ignore_user_config": true,
      "ignore_rules": true
    });
    let active = state
        .bridge
        .lock()
        .map_err(|_| "Connector state is unavailable.")?
        .active_requests
        .clone();
    let response = run_codex_request(payload, &state.codex_bin, &state.workspace, &active)?;
    if response.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        Ok(response
            .get("final_message")
            .and_then(Value::as_str)
            .unwrap_or("Ready")
            .to_string())
    } else {
        Err(response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Codex test failed.")
            .to_string())
    }
}

#[tauri::command]
fn open_codex_install() -> Result<(), String> {
    let url = "https://github.com/openai/codex#quickstart";
    #[cfg(target_os = "macos")]
    let status = Command::new("open").arg(url).status();
    #[cfg(target_os = "windows")]
    let status = Command::new("cmd.exe")
        .args(["/C", "start", "", url])
        .status();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let status = Command::new("xdg-open").arg(url).status();
    status
        .map_err(|error| error.to_string())?
        .success()
        .then_some(())
        .ok_or_else(|| "Unable to open the Codex installation page.".into())
}

#[tauri::command]
fn quit_connector(app: tauri::AppHandle, state: State<'_, ConnectorState>) {
    if let Ok(mut bridge) = state.bridge.lock() {
        bridge.stop();
    }
    app.exit(0);
}

fn main() {
    let background = env::args().any(|argument| argument == "--background");
    let app = tauri::Builder::default()
        .setup(move |app| {
            let codex_bin = find_codex_binary().unwrap_or_else(|| PathBuf::from("codex"));
            let config_dir = app.path().app_config_dir()?;
            let workspace = config_dir.join("workspace");
            fs::create_dir_all(&workspace)?;
            let owner_path = config_dir.join(OWNER_FILE_NAME);
            let owner = Arc::new(Mutex::new(OwnerState {
                binding: load_owner_binding(&owner_path),
                owner_path,
                auth_cache: None,
            }));

            let launch_at_login = load_launch_preference(&config_dir);
            let _ = set_os_autostart(launch_at_login);

            let mut bridge = BridgeService::new();
            if executable_exists(&codex_bin) {
                if let Err(error) =
                    bridge.start(codex_bin.clone(), workspace.clone(), Arc::clone(&owner))
                {
                    bridge.last_error = error;
                }
            } else {
                bridge.last_error = "Codex is not installed.".into();
            }

            app.manage(ConnectorState {
                bridge: Mutex::new(bridge),
                codex_bin,
                workspace,
                config_dir,
                login: Arc::new(Mutex::new(LoginProgress::default())),
                owner,
            });

            if let Some(icon) = app.default_window_icon().cloned() {
                TrayIconBuilder::new()
                    .icon(icon)
                    .tooltip("MarketState Codex Connector")
                    .on_tray_icon_event(|tray, event| {
                        if matches!(
                            event,
                            TrayIconEvent::Click {
                                button: MouseButton::Left,
                                button_state: MouseButtonState::Up,
                                ..
                            }
                        ) {
                            if let Some(window) = tray.app_handle().get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    })
                    .build(app)?;
            }

            if background {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            connector_status,
            set_launch_at_login,
            restart_connector,
            unlink_marketstate_user,
            begin_codex_login,
            test_connection,
            open_codex_install,
            quit_connector
        ])
        .build(tauri::generate_context!())
        .expect("failed to build MarketState Codex Connector");

    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            if let Some(state) = app_handle.try_state::<ConnectorState>() {
                if let Ok(mut bridge) = state.bridge.lock() {
                    bridge.stop();
                }
            }
        }
    });
}
