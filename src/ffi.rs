//! C FFI bindings for the MyLobster library.
//!
//! This module is only compiled when the `ffi` feature is enabled.
//! All functions use `extern "C"` ABI and opaque pointer handles.
//!
//! # Error handling
//! Functions that can fail return `MyLobsterStatus`. On error, a
//! detailed message is stored in thread-local storage and can be
//! retrieved with `mylobster_last_error()`.
//!
//! # Memory management
//! Any `*mut c_char` returned by this library (except `mylobster_version()`)
//! must be freed by calling `mylobster_string_free()`.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Once;

use crate::cli::GatewayOpts;
use crate::config::Config;
use crate::gateway::GatewayServer;

// ============================================================================
// Error handling
// ============================================================================

/// Status codes returned by FFI functions.
#[repr(C)]
pub enum MyLobsterStatus {
    Ok = 0,
    NullPointer = 1,
    InvalidUtf8 = 2,
    ConfigError = 3,
    RuntimeError = 4,
    GatewayError = 5,
    AgentError = 6,
    ChannelError = 7,
    Unknown = 99,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: &str) {
    let c =
        CString::new(msg).unwrap_or_else(|_| CString::new("(error contained null byte)").unwrap());
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = Some(c);
    });
}

/// Retrieve the last error message. Returns a pointer to an internal
/// thread-local buffer — valid until the next FFI call on this thread.
/// Returns null if no error has occurred.
#[no_mangle]
pub extern "C" fn mylobster_last_error() -> *const c_char {
    LAST_ERROR.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(std::ptr::null())
    })
}

/// Clear the last error.
#[no_mangle]
pub extern "C" fn mylobster_clear_error() {
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

// ============================================================================
// Opaque handles
// ============================================================================

/// Opaque handle wrapping a tokio `Runtime`.
pub struct MyLobsterRuntime {
    rt: tokio::runtime::Runtime,
}

/// Opaque handle wrapping a `Config`.
pub struct MyLobsterConfig {
    config: Config,
}

/// Opaque handle wrapping a `GatewayServer`.
///
/// The inner `Option` allows `take()` for the consuming
/// `run_until_shutdown(self)` method.
pub struct MyLobsterGateway {
    server: Option<GatewayServer>,
}

// ============================================================================
// Utility helpers
// ============================================================================

/// Convert a nullable C string to an `Option<&str>`.
/// Returns `Err` if the string is not valid UTF-8.
unsafe fn cstr_to_option(ptr: *const c_char) -> Result<Option<&'static str>, MyLobsterStatus> {
    if ptr.is_null() {
        return Ok(None);
    }
    let cstr = unsafe { CStr::from_ptr(ptr) };
    cstr.to_str()
        .map(Some)
        .map_err(|_| MyLobsterStatus::InvalidUtf8)
}

/// Convert a non-null C string to `&str`.
unsafe fn cstr_to_str(ptr: *const c_char) -> Result<&'static str, MyLobsterStatus> {
    if ptr.is_null() {
        return Err(MyLobsterStatus::NullPointer);
    }
    let cstr = unsafe { CStr::from_ptr(ptr) };
    cstr.to_str().map_err(|_| MyLobsterStatus::InvalidUtf8)
}

/// Allocate a `CString` on the heap and return a raw pointer.
/// The caller must free this with `mylobster_string_free`.
fn string_to_c(s: &str) -> *mut c_char {
    CString::new(s)
        .unwrap_or_else(|_| CString::new("(string contained null byte)").unwrap())
        .into_raw()
}

// ============================================================================
// Runtime lifecycle
// ============================================================================

/// Create a new tokio multi-threaded runtime.
/// Returns null on failure (check `mylobster_last_error()`).
#[no_mangle]
pub extern "C" fn mylobster_runtime_create() -> *mut MyLobsterRuntime {
    match tokio::runtime::Runtime::new() {
        Ok(rt) => Box::into_raw(Box::new(MyLobsterRuntime { rt })),
        Err(e) => {
            set_last_error(&format!("failed to create tokio runtime: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// Destroy a runtime. The pointer must not be used after this call.
///
/// # Safety
/// `rt` must be a valid pointer returned by `mylobster_runtime_create`,
/// or null (in which case this is a no-op).
#[no_mangle]
pub unsafe extern "C" fn mylobster_runtime_destroy(rt: *mut MyLobsterRuntime) {
    if !rt.is_null() {
        drop(unsafe { Box::from_raw(rt) });
    }
}

// ============================================================================
// Config lifecycle
// ============================================================================

/// Load configuration from a file path.
/// Pass null to use default config resolution (searches standard locations).
/// Returns null on failure.
///
/// # Safety
/// `path` must be a valid null-terminated C string or null.
#[no_mangle]
pub unsafe extern "C" fn mylobster_config_load(path: *const c_char) -> *mut MyLobsterConfig {
    let path_opt = match unsafe { cstr_to_option(path) } {
        Ok(p) => p,
        Err(_) => {
            set_last_error("config path is not valid UTF-8");
            return std::ptr::null_mut();
        }
    };

    match Config::load(path_opt) {
        Ok(config) => Box::into_raw(Box::new(MyLobsterConfig { config })),
        Err(e) => {
            set_last_error(&format!("failed to load config: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// Create a configuration with all defaults.
#[no_mangle]
pub extern "C" fn mylobster_config_default() -> *mut MyLobsterConfig {
    Box::into_raw(Box::new(MyLobsterConfig {
        config: Config::default(),
    }))
}

/// Serialize a configuration to JSON.
/// Returns a heap-allocated C string that must be freed with `mylobster_string_free`.
/// Returns null on failure.
///
/// # Safety
/// `config` must be a valid pointer returned by `mylobster_config_load`
/// or `mylobster_config_default`.
#[no_mangle]
pub unsafe extern "C" fn mylobster_config_to_json(config: *const MyLobsterConfig) -> *mut c_char {
    if config.is_null() {
        set_last_error("config pointer is null");
        return std::ptr::null_mut();
    }
    let cfg = unsafe { &(*config).config };
    match serde_json::to_string_pretty(cfg) {
        Ok(json) => string_to_c(&json),
        Err(e) => {
            set_last_error(&format!("failed to serialize config: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// Destroy a configuration handle.
///
/// # Safety
/// `config` must be a valid pointer or null.
#[no_mangle]
pub unsafe extern "C" fn mylobster_config_destroy(config: *mut MyLobsterConfig) {
    if !config.is_null() {
        drop(unsafe { Box::from_raw(config) });
    }
}

// ============================================================================
// Gateway lifecycle
// ============================================================================

/// Start the gateway server.
///
/// `port` — listening port (0 uses config default).
/// `bind_addr` — bind address override (null uses config default).
///
/// Returns null on failure.
///
/// # Safety
/// `runtime` and `config` must be valid pointers. `bind_addr` must be
/// a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn mylobster_gateway_start(
    runtime: *mut MyLobsterRuntime,
    config: *const MyLobsterConfig,
    port: u16,
    bind_addr: *const c_char,
) -> *mut MyLobsterGateway {
    if runtime.is_null() {
        set_last_error("runtime pointer is null");
        return std::ptr::null_mut();
    }
    if config.is_null() {
        set_last_error("config pointer is null");
        return std::ptr::null_mut();
    }

    let rt = unsafe { &(*runtime).rt };
    let cfg = unsafe { (*config).config.clone() };

    let bind_opt = match unsafe { cstr_to_option(bind_addr) } {
        Ok(b) => b.map(|s| s.to_string()),
        Err(_) => {
            set_last_error("bind_addr is not valid UTF-8");
            return std::ptr::null_mut();
        }
    };

    let opts = GatewayOpts {
        config: None,
        port: if port == 0 { None } else { Some(port) },
        bind: bind_opt,
    };

    match rt.block_on(GatewayServer::start(cfg, opts)) {
        Ok(server) => Box::into_raw(Box::new(MyLobsterGateway {
            server: Some(server),
        })),
        Err(e) => {
            set_last_error(&format!("failed to start gateway: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// Run the gateway server until shutdown.
///
/// This function **blocks** the calling thread. Call `mylobster_gateway_shutdown()`
/// from another thread to trigger graceful shutdown.
///
/// # Safety
/// `runtime` and `gateway` must be valid pointers. The gateway can only be
/// run once — subsequent calls return `RuntimeError`.
#[no_mangle]
pub unsafe extern "C" fn mylobster_gateway_run(
    runtime: *mut MyLobsterRuntime,
    gateway: *mut MyLobsterGateway,
) -> MyLobsterStatus {
    if runtime.is_null() {
        set_last_error("runtime pointer is null");
        return MyLobsterStatus::NullPointer;
    }
    if gateway.is_null() {
        set_last_error("gateway pointer is null");
        return MyLobsterStatus::NullPointer;
    }

    let rt = unsafe { &(*runtime).rt };
    let gw = unsafe { &mut *gateway };

    let server = match gw.server.take() {
        Some(s) => s,
        None => {
            set_last_error("gateway has already been run or was consumed");
            return MyLobsterStatus::RuntimeError;
        }
    };

    match rt.block_on(server.run_until_shutdown()) {
        Ok(()) => MyLobsterStatus::Ok,
        Err(e) => {
            set_last_error(&format!("gateway error: {e}"));
            MyLobsterStatus::GatewayError
        }
    }
}

/// Trigger graceful shutdown of the gateway.
/// Safe to call from any thread.
///
/// # Safety
/// `gateway` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn mylobster_gateway_shutdown(
    gateway: *mut MyLobsterGateway,
) -> MyLobsterStatus {
    if gateway.is_null() {
        set_last_error("gateway pointer is null");
        return MyLobsterStatus::NullPointer;
    }

    let gw = unsafe { &*gateway };
    if let Some(ref server) = gw.server {
        server.shutdown();
    }
    // If server was already taken (run completed), shutdown is a no-op.
    MyLobsterStatus::Ok
}

/// Get the address the gateway is listening on.
/// Returns a heap-allocated string like `"127.0.0.1:18789"`.
/// Must be freed with `mylobster_string_free`.
///
/// # Safety
/// `gateway` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn mylobster_gateway_addr(gateway: *const MyLobsterGateway) -> *mut c_char {
    if gateway.is_null() {
        set_last_error("gateway pointer is null");
        return std::ptr::null_mut();
    }

    let gw = unsafe { &*gateway };
    match &gw.server {
        Some(server) => string_to_c(&server.addr().to_string()),
        None => {
            set_last_error("gateway server has been consumed by run");
            std::ptr::null_mut()
        }
    }
}

/// Destroy a gateway handle.
///
/// # Safety
/// `gateway` must be a valid pointer or null.
#[no_mangle]
pub unsafe extern "C" fn mylobster_gateway_destroy(gateway: *mut MyLobsterGateway) {
    if !gateway.is_null() {
        drop(unsafe { Box::from_raw(gateway) });
    }
}

// ============================================================================
// Agent
// ============================================================================

/// Run a single message through the agent pipeline.
///
/// Returns the agent's response as a heap-allocated C string.
/// Must be freed with `mylobster_string_free`.
/// Returns null on failure.
///
/// # Safety
/// `runtime`, `config`, and `message` must be valid pointers.
/// `session_key` may be null.
#[no_mangle]
pub unsafe extern "C" fn mylobster_agent_message(
    runtime: *mut MyLobsterRuntime,
    config: *const MyLobsterConfig,
    message: *const c_char,
    session_key: *const c_char,
) -> *mut c_char {
    if runtime.is_null() {
        set_last_error("runtime pointer is null");
        return std::ptr::null_mut();
    }
    if config.is_null() {
        set_last_error("config pointer is null");
        return std::ptr::null_mut();
    }

    let rt = unsafe { &(*runtime).rt };
    let cfg = unsafe { &(*config).config };

    let msg = match unsafe { cstr_to_str(message) } {
        Ok(s) => s,
        Err(_) => {
            set_last_error("message is null or not valid UTF-8");
            return std::ptr::null_mut();
        }
    };

    let _session = match unsafe { cstr_to_option(session_key) } {
        Ok(s) => s,
        Err(_) => {
            set_last_error("session_key is not valid UTF-8");
            return std::ptr::null_mut();
        }
    };

    // Run the agent and capture the response text.
    // `run_single_message` currently prints to stdout. We use the same
    // provider path but capture the result directly.
    let result = rt.block_on(async {
        let model = cfg
            .agent
            .model
            .primary_model()
            .unwrap_or_else(|| "claude-opus-4".to_string());

        let provider = crate::providers::resolve_provider(cfg, &model)?;

        let messages = vec![crate::providers::ProviderMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(msg.to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let request = crate::providers::ProviderRequest {
            model,
            messages,
            max_tokens: None,
            temperature: None,
            stream: false,
            tools: None,
            tool_choice: None,
        };

        let response = provider.chat(request).await?;
        Ok::<String, anyhow::Error>(response.content_text())
    });

    match result {
        Ok(text) => string_to_c(&text),
        Err(e) => {
            set_last_error(&format!("agent error: {e}"));
            std::ptr::null_mut()
        }
    }
}

// ============================================================================
// Channels
// ============================================================================

/// Send a message through a specific channel (e.g. "telegram", "discord").
///
/// # Safety
/// `runtime`, `config`, `channel`, `to`, and `message` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn mylobster_channel_send(
    runtime: *mut MyLobsterRuntime,
    config: *const MyLobsterConfig,
    channel: *const c_char,
    to: *const c_char,
    message: *const c_char,
) -> MyLobsterStatus {
    if runtime.is_null() || config.is_null() {
        set_last_error("runtime or config pointer is null");
        return MyLobsterStatus::NullPointer;
    }

    let rt = unsafe { &(*runtime).rt };
    let cfg = unsafe { &(*config).config };

    let channel_str = match unsafe { cstr_to_str(channel) } {
        Ok(s) => s,
        Err(status) => {
            set_last_error("channel is null or not valid UTF-8");
            return status;
        }
    };

    let to_str = match unsafe { cstr_to_str(to) } {
        Ok(s) => s,
        Err(status) => {
            set_last_error("to is null or not valid UTF-8");
            return status;
        }
    };

    let msg_str = match unsafe { cstr_to_str(message) } {
        Ok(s) => s,
        Err(status) => {
            set_last_error("message is null or not valid UTF-8");
            return status;
        }
    };

    match rt.block_on(crate::channels::send_message(
        cfg,
        channel_str,
        to_str,
        msg_str,
    )) {
        Ok(()) => MyLobsterStatus::Ok,
        Err(e) => {
            set_last_error(&format!("channel send error: {e}"));
            MyLobsterStatus::ChannelError
        }
    }
}

// ============================================================================
// Chat Completion (JSON-in / JSON-out)
// ============================================================================

/// Perform an OpenAI-compatible chat completion.
///
/// `request_json` — a JSON string conforming to the OpenAI chat completion
/// request schema (model, messages, etc.).
///
/// Returns the completion response as a JSON string. Must be freed with
/// `mylobster_string_free`. Returns null on failure.
///
/// # Safety
/// `runtime`, `config`, and `request_json` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn mylobster_chat_completion(
    runtime: *mut MyLobsterRuntime,
    config: *const MyLobsterConfig,
    request_json: *const c_char,
) -> *mut c_char {
    if runtime.is_null() || config.is_null() {
        set_last_error("runtime or config pointer is null");
        return std::ptr::null_mut();
    }

    let rt = unsafe { &(*runtime).rt };
    let cfg = unsafe { &(*config).config };

    let json_str = match unsafe { cstr_to_str(request_json) } {
        Ok(s) => s,
        Err(_) => {
            set_last_error("request_json is null or not valid UTF-8");
            return std::ptr::null_mut();
        }
    };

    let req: crate::gateway::ChatCompletionRequest = match serde_json::from_str(json_str) {
        Ok(r) => r,
        Err(e) => {
            set_last_error(&format!("invalid request JSON: {e}"));
            return std::ptr::null_mut();
        }
    };

    let sessions = crate::sessions::SessionStore::new(cfg);

    match rt.block_on(crate::agents::handle_chat_completion(cfg, &sessions, req)) {
        Ok(resp) => match serde_json::to_string(&resp) {
            Ok(json) => string_to_c(&json),
            Err(e) => {
                set_last_error(&format!("failed to serialize response: {e}"));
                std::ptr::null_mut()
            }
        },
        Err(e) => {
            set_last_error(&format!("chat completion error: {e}"));
            std::ptr::null_mut()
        }
    }
}

// ============================================================================
// Utility
// ============================================================================

/// Return the library version string.
/// This returns a pointer to a static string — do NOT free it.
#[no_mangle]
pub extern "C" fn mylobster_version() -> *const c_char {
    // The trailing \0 is included by the concat! + c"..." syntax in older
    // Rust. For maximum compatibility we use a static byte array.
    static VERSION: once_cell::sync::Lazy<CString> =
        once_cell::sync::Lazy::new(|| CString::new(env!("CARGO_PKG_VERSION")).unwrap());
    VERSION.as_ptr()
}

/// Initialize the tracing/logging subsystem.
/// Safe to call multiple times — subsequent calls are no-ops.
#[no_mangle]
pub extern "C" fn mylobster_logging_init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        crate::logging::init();
    });
}

/// Free a string previously returned by this library.
///
/// # Safety
/// `s` must be a pointer returned by a `mylobster_*` function that
/// documents it must be freed, or null (no-op).
#[no_mangle]
pub unsafe extern "C" fn mylobster_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}
