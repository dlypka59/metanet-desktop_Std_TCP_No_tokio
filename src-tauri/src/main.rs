#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::{
    net::{TcpListener, TcpStream},
    io::{Read, Write},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tauri::{Manager, Listener, Emitter, AppHandle, WebviewWindow};
use tokio::sync::oneshot;
use log::{info, error};
use env_logger;
use httparse;

static MAIN_WINDOW_NAME: &str = "main";

/// Payload sent from Rust to the frontend for each HTTP request.
#[derive(Serialize)]
struct HttpRequestEvent {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
    request_id: u64,
}

/// Expected payload sent back from the frontend.
#[derive(Deserialize, Debug)]
struct TsResponse {
    request_id: u64,
    status: u16,
    body: String,
}

/// A type alias for our concurrent map of pending responses.
type PendingMap = DashMap<u64, oneshot::Sender<TsResponse>>;

/// Tauri COMMANDS for focus management
#[tauri::command]
fn is_focused(window: WebviewWindow) -> bool {
    match window.is_focused() {
        Ok(focused) => focused,
        Err(_) => false,
    }
}

#[tauri::command]
fn request_focus(window: WebviewWindow) {
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = window.unminimize() { eprintln!("(macOS) unminimize error: {}", e); }
        if let Err(e) = window.request_user_attention(Some(tauri::UserAttentionType::Critical)) {
            eprintln!("(macOS) request_user_attention error: {}", e);
        }
        if let Err(e) = window.set_focus() { eprintln!("(macOS) set_focus error: {}", e); }
    }
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = window.set_focus() { eprintln!("(Windows) set_focus error: {}", e); }
    }
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = window.set_focus() { eprintln!("(Linux) set_focus error: {}", e); }
        if let Err(e) = window.unminimize() { eprintln!("(Linux) unminimize error: {}", e); }
    }
}

#[tauri::command]
fn relinquish_focus(window: WebviewWindow) {
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = window.minimize() { eprintln!("(macOS) hide error: {}", e); }
    }
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = window.minimize() { eprintln!("(Windows) minimize error: {}", e); }
    }
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = window.minimize() { eprintln!("(Linux) minimize error: {}", e); }
    }
}

// Parse an HTTP request using httparse
fn parse_http_request(stream: &mut TcpStream) -> Result<(String, String, Vec<(String, String)>, String), String> {
    let mut buffer = Vec::new();
    let mut temp_buffer = [0; 1024];
    
    loop {
        let bytes_read = stream.read(&mut temp_buffer).map_err(|e| format!("Failed to read from stream: {}", e))?;
        buffer.extend_from_slice(&temp_buffer[..bytes_read]);
        
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        
        match req.parse(&buffer) {
            Ok(httparse::Status::Complete(header_len)) => {
                let method = req.method.unwrap_or("").to_string();
                let path = req.path.unwrap_or("/").to_string();
                
                let headers: Vec<(String, String)> = req.headers.iter()
                    .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).to_string()))
                    .collect();

                let content_length = headers.iter()
                    .find(|(k, _)| k.to_lowercase() == "content-length")
                    .and_then(|(_, v)| v.parse::<usize>().ok())
                    .unwrap_or(0);

                let body_start = header_len;
                let total_len = body_start + content_length;
                if buffer.len() >= total_len {
                    let body = String::from_utf8_lossy(&buffer[body_start..total_len]).to_string();
                    return Ok((method, path, headers, body));
                } else {
                    while buffer.len() < total_len {
                        let bytes_read = stream.read(&mut temp_buffer)
                            .map_err(|e| format!("Failed to read body: {}", e))?;
                        buffer.extend_from_slice(&temp_buffer[..bytes_read]);
                    }
                    let body = String::from_utf8_lossy(&buffer[body_start..total_len]).to_string();
                    return Ok((method, path, headers, body));
                }
            }
            Ok(httparse::Status::Partial) => continue,
            Err(e) => return Err(format!("Failed to parse HTTP request: {:?}", e)),
        }
    }
}

fn handle_client(
    mut stream: TcpStream,
    pending_requests: Arc<PendingMap>,
    app_handle: AppHandle,
    request_counter: Arc<AtomicU64>,
) {
    let peer_addr = stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
    info!("New connection from: {}", peer_addr);

    match parse_http_request(&mut stream) {
        Ok((method, path, headers, body)) => {
            if method == "OPTIONS" {
                let response = "HTTP/1.1 200 OK\r\n\
                                Access-Control-Allow-Origin: *\r\n\
                                Access-Control-Allow-Headers: *\r\n\
                                Access-Control-Allow-Methods: *\r\n\
                                Access-Control-Expose-Headers: *\r\n\
                                Access-Control-Allow-Private-Network: true\r\n\
                                Content-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
                return;
            }

            let request_id = request_counter.fetch_add(1, Ordering::Relaxed);
            info!("Request #{}: {} {}", request_id, method, path);

            let (tx, rx) = oneshot::channel::<TsResponse>();
            pending_requests.insert(request_id, tx);

            let event_payload = HttpRequestEvent {
                method,
                path,
                headers,
                body,
                request_id,
            };

            match serde_json::to_string(&event_payload) {
                Ok(event_json) => {
                    // Fix: Use emit_to with 3 arguments to target the "main" window
                    if let Err(err) = app_handle.emit_to(MAIN_WINDOW_NAME, "http-request", event_json) {
                        error!("Failed to emit http-request event: {:?}", err);
                        pending_requests.remove(&request_id);
                        let response = "HTTP/1.1 500 Internal Server Error\r\n\
                                        Access-Control-Allow-Origin: *\r\n\
                                        Access-Control-Allow-Headers: *\r\n\
                                        Access-Control-Allow-Methods: *\r\n\
                                        Access-Control-Expose-Headers: *\r\n\
                                        Access-Control-Allow-Private-Network: true\r\n\
                                        Content-Length: 21\r\n\r\nInternal Server Error";
                        let _ = stream.write_all(response.as_bytes());
                        return;
                    }

                    match rx.blocking_recv() {
                        Ok(ts_response) => {
                            let mut response = format!(
                                "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n",
                                ts_response.status,
                                if ts_response.status == 200 { "OK" } else { "Unknown" },
                                ts_response.body.len()
                            );
                            response.push_str("Access-Control-Allow-Origin: *\r\n");
                            response.push_str("Access-Control-Allow-Headers: *\r\n");
                            response.push_str("Access-Control-Allow-Methods: *\r\n");
                            response.push_str("Access-Control-Expose-Headers: *\r\n");
                            response.push_str("Access-Control-Allow-Private-Network: true\r\n");
                            response.push_str("\r\n");
                            response.push_str(&ts_response.body);

                            if let Err(e) = stream.write_all(response.as_bytes()) {
                                error!("Failed to send response: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("Error awaiting frontend response for request {}: {:?}", request_id, e);
                            let response = "HTTP/1.1 504 Gateway Timeout\r\n\
                                            Access-Control-Allow-Origin: *\r\n\
                                            Access-Control-Allow-Headers: *\r\n\
                                            Access-Control-Allow-Methods: *\r\n\
                                            Access-Control-Expose-Headers: *\r\n\
                                            Access-Control-Allow-Private-Network: true\r\n\
                                            Content-Length: 13\r\n\r\nGateway Timeout";
                            let _ = stream.write_all(response.as_bytes());
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to serialize HTTP event: {:?}", e);
                    let response = "HTTP/1.1 500 Internal Server Error\r\n\
                                    Access-Control-Allow-Origin: *\r\n\
                                    Access-Control-Allow-Headers: *\r\n\
                                    Access-Control-Allow-Methods: *\r\n\
                                    Access-Control-Expose-Headers: *\r\n\
                                    Access-Control-Allow-Private-Network: true\r\n\
                                    Content-Length: 21\r\n\r\nInternal Server Error";
                    let _ = stream.write_all(response.as_bytes());
                }
            }
            pending_requests.remove(&request_id);
        }
        Err(e) => {
            error!("Failed to parse HTTP request: {}", e);
            let response = "HTTP/1.1 400 Bad Request\r\n\
                            Access-Control-Allow-Origin: *\r\n\
                            Access-Control-Allow-Headers: *\r\n\
                            Access-Control-Allow-Methods: *\r\n\
                            Access-Control-Expose-Headers: *\r\n\
                            Access-Control-Allow-Private-Network: true\r\n\
                            Content-Length: 11\r\n\r\nBad Request";
            let _ = stream.write_all(response.as_bytes());
        }
    }
}

fn main() {
    env_logger::init();

    tauri::Builder::default()
        .setup(move |app| {
            let main_window = app.get_webview_window(MAIN_WINDOW_NAME)
                .expect("Main window not found");
            let app_handle = app.handle().clone(); // Clone AppHandle for thread-safe use

            let pending_requests: Arc<PendingMap> = Arc::new(DashMap::new());
            let request_counter = Arc::new(AtomicU64::new(1));

            {
                let pending_requests = pending_requests.clone();
                main_window.listen("ts-response", move |event| {
                    let payload = event.payload();
                    if payload.len() > 0 {
                        match serde_json::from_str::<TsResponse>(payload) {
                            Ok(ts_response) => {
                                if let Some((req_id, tx)) = pending_requests.remove(&ts_response.request_id) {
                                    if let Err(err) = tx.send(ts_response) {
                                        eprintln!("Failed to send response via oneshot channel for request {}: {:?}", req_id, err);
                                    }
                                } else {
                                    eprintln!("Received ts-response for unknown request_id: {}", ts_response.request_id);
                                }
                            }
                            Err(err) => eprintln!("Failed to parse ts-response payload: {:?}", err),
                        }
                    } else {
                        eprintln!("ts-response event did not include a payload ");
                    }
                });
            }

            let listener = TcpListener::bind("127.0.0.1:3321").unwrap_or_else(|e| {
                error!("Failed to bind to 127.0.0.1:3321: {}", e);
                std::process::exit(1);
            });
            info!("Server listening on 127.0.0.1:3321");

            let pending_requests_clone = pending_requests.clone();
            let app_handle_clone = app_handle.clone(); // Use AppHandle instead of Window
            let request_counter_clone = request_counter.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let pending_requests = pending_requests_clone.clone();
                            let app_handle = app_handle_clone.clone();
                            let request_counter = request_counter_clone.clone();
                            thread::spawn(move || {
                                handle_client(stream, pending_requests, app_handle, request_counter);
                            });
                        }
                        Err(e) => error!("Error accepting connection: {}", e),
                    }
                }
            });

            #[cfg(target_os = "macos")]
            {
                let app_handle = app.handle().clone();
                app.listen_any("tauri://reopen", move |_event| {
                    if let Some(window) = app_handle.get_webview_window(MAIN_WINDOW_NAME) {
                        if let Err(e) = window.show() { eprintln!("(macOS) show error: {}", e); }
                        if let Err(e) = window.set_focus() { eprintln!("(macOS) set_focus error: {}", e); }
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            is_focused,
            request_focus,
            relinquish_focus
        ])
        .run(tauri::generate_context!())
        .expect("Error while running Tauri application");
}