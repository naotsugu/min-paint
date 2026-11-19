//! MCP server: lets AI clients drive the paint document over Streamable HTTP.
//!
//! This module has no GUI dependency. A tool call becomes an `McpRequest` that is sent to
//! the UI thread through a channel; the UI runs it on the live document and answers
//! through a oneshot channel.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::Engine as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{select, Either};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::model::{Tool, MAX_CANVAS_SIZE};

// Coordinates are limited so that a request cannot keep the UI thread busy for long.
const COORD_LIMIT: i32 = 2 * MAX_CANVAS_SIZE as i32;
const MAX_BRUSH_SIZE: i32 = 64;
const MAX_TEXT_CHARS: usize = 500;
const DEFAULT_FONT_SIZE: f32 = 24.;
const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 6.0..=500.0;
// how long a tool call waits for the UI (it cannot answer while a native dialog is open)
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

// -- messages between the server thread and the UI -------------------------------

/// Per-call overrides of the painter settings; `None` keeps the application's value.
#[derive(Default)]
pub struct Style {
    pub size: Option<i32>,
    pub color: Option<[u8; 4]>,
    pub filled: Option<bool>,
    pub antialias: Option<bool>,
}

/// An operation for the UI thread. Values are already validated.
pub enum McpOp {
    GetCanvasInfo,
    GetImage,
    ListFonts,
    SetColor([u8; 4]),
    /// line / rectangle / oval / arrow between two canvas pixels
    Shape { tool: Tool, from: (i32, i32), to: (i32, i32), style: Style },
    Fill { at: (i32, i32), style: Style },
    Mosaic { x: i32, y: i32, w: i32, h: i32, block: i32 },
    Text { text: String, at: (i32, i32), font_size: f32, font: Option<String>, style: Style },
    Undo,
    Redo,
    Resize { width: usize, height: usize },
}

impl McpOp {
    /// Operations that change the document or its history.
    pub fn is_edit(&self) -> bool {
        !matches!(self, Self::GetCanvasInfo | Self::GetImage | Self::ListFonts | Self::SetColor(_))
    }
}

pub enum McpReply {
    Done,
    Info(CanvasInfo),
    Png(Vec<u8>),
    Fonts(Vec<String>),
}

pub struct McpRequest {
    pub op: McpOp,
    pub reply: oneshot::Sender<Result<McpReply, String>>,
}

#[derive(Serialize)]
pub struct CanvasInfo {
    pub width: usize,
    pub height: usize,
    /// current drawing color, "#rrggbb"
    pub color: String,
    pub brush_size: i32,
    pub antialias: bool,
    /// true when there are unsaved changes
    pub modified: bool,
    pub file: Option<String>,
}

// -- tool parameters ------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct SetColorParams {
    /// Color as "#rrggbb" (or "#rrggbbaa").
    color: String,
}

/// Parameters of line / rectangle / ellipse / arrow: two points in canvas pixels.
#[derive(Deserialize, schemars::JsonSchema)]
struct ShapeParams {
    /// First point, x in canvas pixels (0 = left edge). The arrow starts here.
    x0: i32,
    /// First point, y in canvas pixels (0 = top edge).
    y0: i32,
    /// Second point, x in canvas pixels. The arrow head is here.
    x1: i32,
    /// Second point, y in canvas pixels.
    y1: i32,
    /// Line width in pixels (1-64); for an arrow, the size of the whole arrow.
    /// Defaults to the current brush size.
    size: Option<i32>,
    /// Color as "#rrggbb". Defaults to the current drawing color.
    color: Option<String>,
    /// Fill the shape (rectangle and ellipse only). Defaults to the current setting.
    filled: Option<bool>,
    /// Anti-aliased edges. Defaults to the current setting.
    antialias: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct FillParams {
    /// x of the clicked pixel in canvas pixels.
    x: i32,
    /// y of the clicked pixel in canvas pixels.
    y: i32,
    /// Color as "#rrggbb". Defaults to the current drawing color.
    color: Option<String>,
    /// Also recolor the anti-aliased pixels around the region. Defaults to the current setting.
    antialias: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MosaicParams {
    /// Left edge of the rectangle in canvas pixels.
    x: i32,
    /// Top edge of the rectangle in canvas pixels.
    y: i32,
    /// Width of the rectangle in pixels.
    width: i32,
    /// Height of the rectangle in pixels.
    height: i32,
    /// Edge length of one mosaic cell in pixels (larger = coarser).
    block_size: i32,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TextParams {
    /// The text to draw (up to 500 characters).
    text: String,
    /// Left edge of the text in canvas pixels.
    x: i32,
    /// Top edge of the text in canvas pixels.
    y: i32,
    /// Font size in pixels (6-500). Defaults to 24.
    font_size: Option<f32>,
    /// Font family name; see list_fonts. Defaults to the font used last, or a system font.
    font: Option<String>,
    /// Color as "#rrggbb". Defaults to the current drawing color.
    color: Option<String>,
    /// Anti-aliased text. Defaults to the current setting.
    antialias: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ResizeParams {
    /// New canvas width in pixels (1-4096).
    width: i32,
    /// New canvas height in pixels (1-4096).
    height: i32,
}

// -- server ---------------------------------------------------------------------

#[derive(Clone)]
struct PaintServer {
    tx: mpsc::UnboundedSender<McpRequest>,
}

impl PaintServer {
    /// Send an operation to the UI thread and wait for its answer.
    async fn call(&self, op: McpOp) -> Result<McpReply, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .unbounded_send(McpRequest { op, reply })
            .map_err(|_| "the application has shut down".to_string())?;
        match tokio::time::timeout(REPLY_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the application dropped the request".to_string()),
            Err(_) => Err("timed out: the application did not respond (is a dialog open?)".to_string()),
        }
    }

    /// Run a validated operation; a validation error becomes a tool error.
    async fn run(&self, op: Result<McpOp, String>) -> Result<CallToolResult, McpError> {
        Ok(match op {
            Ok(op) => into_result(self.call(op).await),
            Err(message) => tool_error(message),
        })
    }
}

#[tool_router]
impl PaintServer {
    #[tool(description = "Get the canvas size, the current drawing color, the brush size \
                          and whether there are unsaved changes.")]
    async fn get_canvas_info(&self) -> Result<CallToolResult, McpError> {
        self.run(Ok(McpOp::GetCanvasInfo)).await
    }

    #[tool(description = "Get the current canvas as a PNG image.")]
    async fn get_image(&self) -> Result<CallToolResult, McpError> {
        self.run(Ok(McpOp::GetImage)).await
    }

    #[tool(description = "List the installed font families that draw_text accepts.")]
    async fn list_fonts(&self) -> Result<CallToolResult, McpError> {
        self.run(Ok(McpOp::ListFonts)).await
    }

    #[tool(description = "Set the current drawing color of the application (\"#rrggbb\").")]
    async fn set_color(&self, Parameters(p): Parameters<SetColorParams>) -> Result<CallToolResult, McpError> {
        self.run(parse_color(&p.color).map(McpOp::SetColor)).await
    }

    #[tool(description = "Draw a straight line. Coordinates are canvas pixels with the origin at \
                          the top-left; parts outside the canvas are clipped. One undo step.")]
    async fn draw_line(&self, Parameters(p): Parameters<ShapeParams>) -> Result<CallToolResult, McpError> {
        self.run(build_shape(Tool::Line, p)).await
    }

    #[tool(description = "Draw a rectangle whose opposite corners are (x0, y0) and (x1, y1). \
                          Outline by default; set filled to fill it. One undo step.")]
    async fn draw_rectangle(&self, Parameters(p): Parameters<ShapeParams>) -> Result<CallToolResult, McpError> {
        self.run(build_shape(Tool::Rectangle, p)).await
    }

    #[tool(description = "Draw an ellipse inscribed in the rectangle whose opposite corners are \
                          (x0, y0) and (x1, y1). Outline by default; set filled to fill it. One undo step.")]
    async fn draw_ellipse(&self, Parameters(p): Parameters<ShapeParams>) -> Result<CallToolResult, McpError> {
        self.run(build_shape(Tool::Oval, p)).await
    }

    #[tool(description = "Draw an arrow from (x0, y0) to (x1, y1); the head is at the end. \
                          size scales the arrow. One undo step.")]
    async fn draw_arrow(&self, Parameters(p): Parameters<ShapeParams>) -> Result<CallToolResult, McpError> {
        self.run(build_shape(Tool::Arrow, p)).await
    }

    #[tool(description = "Flood fill: replace the connected region of the color at (x, y) with \
                          the drawing color. One undo step.")]
    async fn flood_fill(&self, Parameters(p): Parameters<FillParams>) -> Result<CallToolResult, McpError> {
        self.run(build_fill(p)).await
    }

    #[tool(description = "Pixelate a rectangle: every block_size x block_size cell is replaced by \
                          its average color. One undo step.")]
    async fn mosaic(&self, Parameters(p): Parameters<MosaicParams>) -> Result<CallToolResult, McpError> {
        self.run(build_mosaic(p)).await
    }

    #[tool(description = "Draw text with its top-left corner at (x, y). Fails when the position \
                          is outside the canvas. One undo step.")]
    async fn draw_text(&self, Parameters(p): Parameters<TextParams>) -> Result<CallToolResult, McpError> {
        self.run(build_text(p)).await
    }

    #[tool(description = "Undo the last edit. Fails when there is nothing to undo.")]
    async fn undo(&self) -> Result<CallToolResult, McpError> {
        self.run(Ok(McpOp::Undo)).await
    }

    #[tool(description = "Redo the last undone edit. Fails when there is nothing to redo.")]
    async fn redo(&self) -> Result<CallToolResult, McpError> {
        self.run(Ok(McpOp::Redo)).await
    }

    #[tool(description = "Resize the canvas. The top-left corner stays in place: a smaller size crops \
                      the right and bottom edges, a larger size adds a transparent area there. \
                      One undo step.")]
    async fn resize_canvas(&self, Parameters(p): Parameters<ResizeParams>) -> Result<CallToolResult, McpError> {
        self.run(build_resize(p)).await
    }
}

#[tool_handler(
    name = "min-paint",
    instructions = "Draws on the canvas that is open in min-paint. Coordinates are canvas pixels \
                    with the origin at the top-left. Use get_image to look at the result."
)]
impl ServerHandler for PaintServer {}

// -- results --------------------------------------------------------------------

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn json_result<T: Serialize>(value: &T) -> CallToolResult {
    match serde_json::to_string(value) {
        Ok(json) => CallToolResult::success(vec![ContentBlock::text(json)]),
        Err(e) => tool_error(e.to_string()),
    }
}

fn into_result(result: Result<McpReply, String>) -> CallToolResult {
    match result {
        Ok(McpReply::Done) => CallToolResult::success(vec![ContentBlock::text("ok")]),
        Ok(McpReply::Info(info)) => json_result(&info),
        Ok(McpReply::Fonts(fonts)) => json_result(&fonts),
        Ok(McpReply::Png(png)) => {
            let data = base64::engine::general_purpose::STANDARD.encode(png);
            CallToolResult::success(vec![ContentBlock::image(data, "image/png")])
        }
        Err(message) => tool_error(message),
    }
}

// -- validation -----------------------------------------------------------------

/// Parse "#rrggbb" or "#rrggbbaa" (the leading '#' is optional).
pub fn parse_color(text: &str) -> Result<[u8; 4], String> {
    let invalid = || format!("invalid color {text:?}: expected #rrggbb");
    let hex = text.trim().trim_start_matches('#');
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let value = u32::from_str_radix(hex, 16).map_err(|_| invalid())?;
    match hex.len() {
        6 => Ok([(value >> 16) as u8, (value >> 8) as u8, value as u8, 0xff]),
        8 => Ok([(value >> 24) as u8, (value >> 16) as u8, (value >> 8) as u8, value as u8]),
        _ => Err(invalid()),
    }
}

/// Format a color as "#rrggbb" ("#rrggbbaa" when it is not opaque).
pub fn format_color(c: [u8; 4]) -> String {
    if c[3] == 0xff {
        format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3])
    }
}

fn check_point(name: &str, x: i32, y: i32) -> Result<(i32, i32), String> {
    let range = -COORD_LIMIT..=COORD_LIMIT;
    if range.contains(&x) && range.contains(&y) {
        Ok((x, y))
    } else {
        Err(format!("{name} is out of range: coordinates must be between {} and {COORD_LIMIT}", -COORD_LIMIT))
    }
}

fn build_style(
    size: Option<i32>,
    color: Option<&str>,
    filled: Option<bool>,
    antialias: Option<bool>,
) -> Result<Style, String> {
    if let Some(size) = size {
        if !(1..=MAX_BRUSH_SIZE).contains(&size) {
            return Err(format!("size must be between 1 and {MAX_BRUSH_SIZE}"));
        }
    }
    let color = color.map(parse_color).transpose()?;
    Ok(Style { size, color, filled, antialias })
}

fn build_shape(tool: Tool, p: ShapeParams) -> Result<McpOp, String> {
    let from = check_point("(x0, y0)", p.x0, p.y0)?;
    let to = check_point("(x1, y1)", p.x1, p.y1)?;
    let style = build_style(p.size, p.color.as_deref(), p.filled, p.antialias)?;
    Ok(McpOp::Shape { tool, from, to, style })
}

fn build_fill(p: FillParams) -> Result<McpOp, String> {
    let at = check_point("(x, y)", p.x, p.y)?;
    let style = build_style(None, p.color.as_deref(), None, p.antialias)?;
    Ok(McpOp::Fill { at, style })
}

fn build_mosaic(p: MosaicParams) -> Result<McpOp, String> {
    check_point("(x, y)", p.x, p.y)?;
    let max = MAX_CANVAS_SIZE as i32;
    for (name, value) in [("width", p.width), ("height", p.height), ("block_size", p.block_size)] {
        if !(1..=max).contains(&value) {
            return Err(format!("{name} must be between 1 and {max}"));
        }
    }
    Ok(McpOp::Mosaic { x: p.x, y: p.y, w: p.width, h: p.height, block: p.block_size })
}

fn build_text(p: TextParams) -> Result<McpOp, String> {
    if p.text.is_empty() || p.text.chars().count() > MAX_TEXT_CHARS {
        return Err(format!("text must be 1 to {MAX_TEXT_CHARS} characters long"));
    }
    let at = check_point("(x, y)", p.x, p.y)?;
    let font_size = p.font_size.unwrap_or(DEFAULT_FONT_SIZE);
    if !FONT_SIZE_RANGE.contains(&font_size) {
        return Err(format!(
            "font_size must be between {} and {}",
            FONT_SIZE_RANGE.start(),
            FONT_SIZE_RANGE.end()
        ));
    }
    let style = build_style(None, p.color.as_deref(), None, p.antialias)?;
    Ok(McpOp::Text { text: p.text, at, font_size, font: p.font, style })
}

fn build_resize(p: ResizeParams) -> Result<McpOp, String> {
    let max = MAX_CANVAS_SIZE as i32;
    for (name, value) in [("width", p.width), ("height", p.height)] {
        if !(1..=max).contains(&value) {
            return Err(format!("{name} must be between 1 and {max}"));
        }
    }
    Ok(McpOp::Resize { width: p.width as usize, height: p.height as usize })
}

// -- settings and registration examples (used by the settings dialog) ---------------

pub const DEFAULT_PORT: u16 = 8787;
/// the name the server is registered under in MCP clients
pub const SERVER_NAME: &str = "min-paint";
const MIN_PORT: u16 = 1024;
const MIN_TOKEN_LEN: usize = 16;
const MAX_TOKEN_LEN: usize = 128;

/// Where the running server listens and the token it requires.
#[derive(Clone)]
pub struct McpEndpoint {
    pub port: u16,
    pub token: String,
}

/// A random token (32 hex digits) for the dialog's default value.
/// Returns an empty string when the OS has no random number generator.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        return String::new();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Default token of the dialog. It is derived from the name and the creation time of the
/// home directory, so it is the same on every run and clients need to be registered only once.
/// Falls back to a random token when the home directory or its creation time is unknown.
pub fn default_token() -> String {
    derive_token().unwrap_or_else(generate_token)
}

fn derive_token() -> Option<String> {
    let home = home_dir()?;
    let name = home.file_name()?.to_string_lossy().into_owned();
    // not every platform / file system records a creation time
    let created = std::fs::metadata(&home).ok()?.created().ok()?;
    let secs = created.duration_since(UNIX_EPOCH).ok()?.as_secs();

    let mut hasher = Sha256::new();
    // the version tag lets a future change of this scheme produce different tokens
    hasher.update(b"min-paint-mcp-token-v1");
    hasher.update([0u8]);
    hasher.update(name.as_bytes());
    hasher.update([0u8]);
    hasher.update(secs.to_be_bytes());
    // 16 bytes = 32 hex digits, which `parse_settings` accepts
    Some(hasher.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect())
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_dir())
}

/// Validate the values typed in the dialog.
pub fn parse_settings(port: &str, token: &str) -> Result<(u16, String), String> {
    let port: u16 = port
        .trim()
        .parse()
        .ok()
        .filter(|p| *p >= MIN_PORT)
        .ok_or_else(|| format!("The port must be a number between {MIN_PORT} and 65535."))?;
    // only characters that need no escaping in a header, a shell command or JSON
    let token = token.trim();
    let valid_char = |c: char| c.is_ascii_alphanumeric() || "-._~".contains(c);
    if !(MIN_TOKEN_LEN..=MAX_TOKEN_LEN).contains(&token.len()) || !token.chars().all(valid_char) {
        return Err(format!(
            "The token must be {MIN_TOKEN_LEN} to {MAX_TOKEN_LEN} characters: \
             letters, digits, '-', '.', '_' or '~'."
        ));
    }
    Ok((port, token.to_string()))
}

pub fn endpoint_url(port: &str) -> String {
    format!("http://127.0.0.1:{port}/mcp")
}

/// Command that registers the server in Claude Code.
pub fn claude_code_command(port: &str, token: &str) -> String {
    format!(
        "claude mcp add --transport http {SERVER_NAME} {} --header \"Authorization: Bearer {token}\"",
        endpoint_url(port)
    )
}

/// Entry for the `mcp_config.json` of clients that read `mcpServers` with `url` and `headers`.
pub fn config_json(port: &str, token: &str) -> String {
    format!(
        "{{\n  \"mcpServers\": {{\n    \"{SERVER_NAME}\": {{\n      \"url\": \"{}\",\n      \"headers\": {{\n        \"Authorization\": \"Bearer {token}\"\n      }}\n    }}\n  }}\n}}",
        endpoint_url(port)
    )
}

// -- startup --------------------------------------------------------------------

/// A running server. Dropping it stops the server and releases the port.
pub struct McpServer {
    pub endpoint: McpEndpoint,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for McpServer {
    fn drop(&mut self) {
        // tell the server thread to stop
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // wait until the port is released, so that the server can be started again at once
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start the server on 127.0.0.1:`port`. Requests need `Authorization: Bearer <token>`.
/// Returns the handle (drop it to stop the server) and the receiver that the UI has to drain;
/// the receiver ends by itself when the server stops. The port is bound before this returns,
/// so that a failure (for example "port in use") can be reported to the user.
pub fn start(
    port: u16,
    token: String,
) -> std::io::Result<(McpServer, mpsc::UnboundedReceiver<McpRequest>)> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    listener.set_nonblocking(true)?;
    let (tx, rx) = mpsc::unbounded();
    let (stop, stop_rx) = oneshot::channel::<()>();
    let endpoint = McpEndpoint { port, token: token.clone() };

    let thread = std::thread::Builder::new()
        .name("mcp-server".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    eprintln!("mcp: failed to create the runtime: {e}");
                    return;
                }
            };
            let result = runtime.block_on(async move {
                let server = std::pin::pin!(serve(listener, tx, token));
                match select(server, stop_rx).await {
                    // the server ended by itself
                    Either::Left((result, _)) => result,
                    // stop requested
                    Either::Right(_) => Ok(()),
                }
            });
            if let Err(e) = result {
                eprintln!("mcp: server stopped: {e}");
            }
            // dropping the runtime cancels the open connections
            drop(runtime);
        })?;

    Ok((McpServer { endpoint, stop: Some(stop), thread: Some(thread) }, rx))
}

async fn serve(
    listener: TcpListener,
    tx: mpsc::UnboundedSender<McpRequest>,
    token: String,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::from_std(listener)?;
    // the default config only accepts loopback Host headers (DNS rebinding protection)
    let service = StreamableHttpService::new(
        move || Ok(PaintServer { tx: tx.clone() }),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(Arc::new(token), require_bearer));
    axum::serve(listener, app).await
}

async fn require_bearer(State(token): State<Arc<String>>, request: Request, next: Next) -> Response {
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| constant_time_eq(given.as_bytes(), token.as_bytes()));
    if authorized {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
