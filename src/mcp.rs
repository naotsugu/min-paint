//! MCP server: lets AI clients drive the paint document over Streamable HTTP.
//!
//! This module has no GUI dependency. A tool call becomes an `McpRequest` that is sent to
//! the UI thread through a channel; the UI runs it on the live document and answers
//! through a oneshot channel.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::Engine as _;
use futures::channel::{mpsc, oneshot};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::model::MAX_CANVAS_SIZE;

// Coordinates are limited so that a request cannot keep the UI thread busy for long.
const COORD_LIMIT: i32 = 2 * MAX_CANVAS_SIZE as i32;
const MAX_BRUSH_SIZE: i32 = 64;
// how long a tool call waits for the UI (it cannot answer while a native dialog is open)
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

// -- messages between the server thread and the UI -------------------------------

/// An operation for the UI thread. Values are already validated.
pub enum McpOp {
    GetCanvasInfo,
    GetImage,
    SetColor([u8; 4]),
    DrawLine {
        from: (i32, i32),
        to: (i32, i32),
        size: Option<i32>,
        color: Option<[u8; 4]>,
    },
}

pub enum McpReply {
    Done,
    Info(CanvasInfo),
    Png(Vec<u8>),
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

#[derive(Deserialize, schemars::JsonSchema)]
struct DrawLineParams {
    /// Start point, x in canvas pixels (0 = left edge).
    x0: i32,
    /// Start point, y in canvas pixels (0 = top edge).
    y0: i32,
    /// End point, x in canvas pixels.
    x1: i32,
    /// End point, y in canvas pixels.
    y1: i32,
    /// Line width in pixels (1-64). Defaults to the current brush size.
    size: Option<i32>,
    /// Color as "#rrggbb". Defaults to the current drawing color.
    color: Option<String>,
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
}

#[tool_router]
impl PaintServer {
    #[tool(description = "Get the canvas size, the current drawing color, the brush size \
                          and whether there are unsaved changes.")]
    async fn get_canvas_info(&self) -> Result<CallToolResult, McpError> {
        Ok(into_result(self.call(McpOp::GetCanvasInfo).await))
    }

    #[tool(description = "Get the current canvas as a PNG image.")]
    async fn get_image(&self) -> Result<CallToolResult, McpError> {
        Ok(into_result(self.call(McpOp::GetImage).await))
    }

    #[tool(description = "Set the current drawing color of the application (\"#rrggbb\").")]
    async fn set_color(&self, Parameters(p): Parameters<SetColorParams>) -> Result<CallToolResult, McpError> {
        match parse_color(&p.color) {
            Ok(color) => Ok(into_result(self.call(McpOp::SetColor(color)).await)),
            Err(message) => Ok(tool_error(message)),
        }
    }

    #[tool(description = "Draw a straight line. Coordinates are canvas pixels with the origin at \
                          the top-left; points outside the canvas are clipped. The line is one undo step.")]
    async fn draw_line(&self, Parameters(p): Parameters<DrawLineParams>) -> Result<CallToolResult, McpError> {
        match build_draw_line(p) {
            Ok(op) => Ok(into_result(self.call(op).await)),
            Err(message) => Ok(tool_error(message)),
        }
    }
}

#[tool_handler(
    name = "min-paint",
    instructions = "Draws on the canvas that is open in min-paint. Coordinates are canvas pixels \
                    with the origin at the top-left. Use get_image to look at the result."
)]
impl ServerHandler for PaintServer {}

// -- helpers --------------------------------------------------------------------

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn into_result(result: Result<McpReply, String>) -> CallToolResult {
    match result {
        Ok(McpReply::Done) => CallToolResult::success(vec![ContentBlock::text("ok")]),
        Ok(McpReply::Info(info)) => match serde_json::to_string(&info) {
            Ok(json) => CallToolResult::success(vec![ContentBlock::text(json)]),
            Err(e) => tool_error(e.to_string()),
        },
        Ok(McpReply::Png(png)) => {
            let data = base64::engine::general_purpose::STANDARD.encode(png);
            CallToolResult::success(vec![ContentBlock::image(data, "image/png")])
        }
        Err(message) => tool_error(message),
    }
}

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

fn build_draw_line(p: DrawLineParams) -> Result<McpOp, String> {
    let from = check_point("(x0, y0)", p.x0, p.y0)?;
    let to = check_point("(x1, y1)", p.x1, p.y1)?;
    if let Some(size) = p.size {
        if !(1..=MAX_BRUSH_SIZE).contains(&size) {
            return Err(format!("size must be between 1 and {MAX_BRUSH_SIZE}"));
        }
    }
    let color = p.color.as_deref().map(parse_color).transpose()?;
    Ok(McpOp::DrawLine { from, to, size: p.size, color })
}

// -- startup --------------------------------------------------------------------

/// Start the server on 127.0.0.1:`port`. Requests need `Authorization: Bearer <token>`.
/// Returns the receiver that the UI has to drain; the port is bound before this returns,
/// so that a failure (for example "port in use") can be reported to the user.
pub fn start(port: u16, token: String) -> std::io::Result<mpsc::UnboundedReceiver<McpRequest>> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    listener.set_nonblocking(true)?;
    let (tx, rx) = mpsc::unbounded();

    std::thread::Builder::new()
        .name("mcp-server".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    eprintln!("mcp: failed to create the runtime: {e}");
                    return;
                }
            };
            if let Err(e) = runtime.block_on(serve(listener, tx, token)) {
                eprintln!("mcp: server stopped: {e}");
            }
        })?;
    Ok(rx)
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
