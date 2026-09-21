//! `engine ui` — a localhost test UI. Zero new dependencies: a minimal
//! synchronous HTTP/1.1 server (localhost only; serial handling doubles
//! as a natural render queue), plus an embedded page for editing
//! markup, checking it, rendering, and watching the result.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

use scene_ir::{Diagnostic, Severity};

use crate::commands::{self, DiagBundle};

const PAGE: &str = include_str!("ui.html");
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 4 * 1024 * 1024;

pub fn serve(dir: &Path, port: u16, open_browser: bool) -> i32 {
    let dir = match dir.canonicalize() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: project dir {}: {e}", dir.display());
            return 1;
        }
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot bind 127.0.0.1:{port}: {e}");
            return 1;
        }
    };
    let url = format!("http://127.0.0.1:{port}/");
    println!("scene engine ui — {url}");
    println!("  project: {}", dir.display());
    println!("  ctrl-c to stop");
    if open_browser {
        open_url(&url);
    }
    for stream in listener.incoming().flatten() {
        handle(stream, &dir);
    }
    0
}

#[cfg(target_os = "macos")]
fn open_url(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}
#[cfg(target_os = "windows")]
fn open_url(url: &str) {
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn();
}
#[cfg(all(unix, not(target_os = "macos")))]
fn open_url(url: &str) {
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

// --- HTTP plumbing -------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Response payload: small bodies stay in memory; file bodies stream in
/// chunks so a plain GET on a large mp4 never buffers it whole.
enum Body {
    Bytes(Vec<u8>),
    File { file: std::fs::File, remaining: u64 },
}

struct Response {
    status: u16,
    content_type: &'static str,
    /// Content-Length — known upfront for both body kinds.
    body_len: u64,
    body: Body,
    extra_headers: Vec<String>,
}

impl Response {
    fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Response {
            status,
            content_type,
            body_len: body.len() as u64,
            body: Body::Bytes(body),
            extra_headers: Vec::new(),
        }
    }
    fn json(v: serde_json::Value) -> Self {
        Response::new(200, "application/json", v.to_string().into_bytes())
    }
}

/// A total request deadline also bounds clients that trickle one byte at a time.
struct RequestReader<'a> {
    stream: &'a mut TcpStream,
    deadline: std::time::Instant,
}
impl Read for RequestReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))?;
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(bytes)
    }
}

fn handle(mut stream: TcpStream, dir: &Path) {
    // A stalled client must not hang the serial server.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
    let request = read_request(&mut RequestReader {
        stream: &mut stream,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(10),
    });
    let (res, is_head) = match request {
        Ok(Some(req)) => (route(&req, dir), req.method == "HEAD"),
        Ok(None) => return,
        Err(error) => {
            let status = match error.kind() {
                std::io::ErrorKind::FileTooLarge => 413,
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => 408,
                _ => 400,
            };
            (
                Response::new(status, "text/plain", error.to_string().into_bytes()),
                false,
            )
        }
    };
    let status_text = match res.status {
        200 => "200 OK",
        206 => "206 Partial Content",
        400 => "400 Bad Request",
        408 => "408 Request Timeout",
        413 => "413 Content Too Large",
        403 => "403 Forbidden",
        404 => "404 Not Found",
        415 => "415 Unsupported Media Type",
        _ => "500 Internal Server Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status_text}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        res.content_type, res.body_len
    );
    for h in &res.extra_headers {
        head.push_str(h);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    if is_head {
        return;
    }
    match res.body {
        Body::Bytes(b) => {
            let _ = stream.write_all(&b);
        }
        Body::File {
            mut file,
            remaining,
        } => {
            let mut buf = [0u8; 64 * 1024];
            let mut left = remaining;
            while left > 0 {
                let want = left.min(buf.len() as u64) as usize;
                match file.read(&mut buf[..want]) {
                    Ok(0) | Err(_) => break, // EOF or error mid-body: close
                    Ok(n) => {
                        if stream.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        left -= n as u64;
                    }
                }
            }
        }
    }
}

fn bad_request(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn too_large() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::FileTooLarge, "request exceeds limit")
}

fn read_line(reader: &mut impl Read, budget: &mut usize) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    loop {
        if *budget == 0 {
            return Err(too_large());
        }
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        *budget -= 1;
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            if !bytes.ends_with(b"\r\n") {
                return Err(bad_request("expected CRLF"));
            }
            bytes.truncate(bytes.len() - 2);
            return String::from_utf8(bytes).map_err(|_| bad_request("invalid header encoding"));
        }
    }
}

fn header(line: &str) -> std::io::Result<(String, String)> {
    let (name, value) = line
        .split_once(':')
        .ok_or_else(|| bad_request("invalid header"))?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        || value.bytes().any(|b| b < 32 && b != b'\t' || b == 127)
    {
        return Err(bad_request("invalid header"));
    }
    Ok((name.to_ascii_lowercase(), value.trim().to_string()))
}

fn valid_chunk_extensions(mut input: &str) -> bool {
    fn token(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
    }
    while !input.is_empty() {
        let Some(rest) = input.strip_prefix(';') else {
            return false;
        };
        input = rest.trim_start_matches([' ', '\t']);
        let n = input.bytes().take_while(|b| token(*b)).count();
        if n == 0 {
            return false;
        }
        input = input[n..].trim_start_matches([' ', '\t']);
        if let Some(rest) = input.strip_prefix('=') {
            input = rest.trim_start_matches([' ', '\t']);
            if let Some(rest) = input.strip_prefix('"') {
                let mut escaped = false;
                let mut end = None;
                for (i, byte) in rest.bytes().enumerate() {
                    if byte < 32 && byte != b'\t' || byte == 127 {
                        return false;
                    }
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        end = Some(i);
                        break;
                    }
                }
                let Some(end) = end else {
                    return false;
                };
                input = &rest[end + 1..];
            } else {
                let n = input.bytes().take_while(|b| token(*b)).count();
                if n == 0 {
                    return false;
                }
                input = &input[n..];
            }
            input = input.trim_start_matches([' ', '\t']);
        }
    }
    true
}

fn read_request(stream: &mut impl Read) -> std::io::Result<Option<Request>> {
    let mut reader = std::io::BufReader::new(stream);
    let mut budget = MAX_HEAD;
    let line = read_line(&mut reader, &mut budget)?;
    let parts: Vec<_> = line.split(' ').collect();
    if parts.len() != 3
        || !matches!(parts[2], "HTTP/1.0" | "HTTP/1.1")
        || parts[0].is_empty()
        || !parts[1].starts_with('/')
    {
        return Err(bad_request("invalid request line"));
    }
    let (method, path) = (parts[0].to_string(), parts[1].to_string());
    let mut headers = Vec::new();
    let mut length = None;
    let mut chunked = false;
    loop {
        let line = read_line(&mut reader, &mut budget)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = header(&line)?;
        match name.as_str() {
            "content-length" => {
                if length.is_some()
                    || value.is_empty()
                    || !value.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(bad_request("invalid or repeated content-length"));
                }
                let n = value.parse::<usize>().map_err(|_| too_large())?;
                if n > MAX_BODY {
                    return Err(too_large());
                }
                length = Some(n);
            }
            "transfer-encoding" => {
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err(bad_request("unsupported transfer-encoding"));
                }
                chunked = true;
            }
            _ => {}
        }
        headers.push((name, value));
    }
    if chunked && length.is_some() {
        return Err(bad_request("conflicting request framing"));
    }
    let mut body = Vec::new();
    if chunked {
        let mut framing_budget = MAX_HEAD;
        loop {
            let mut line_budget = 8192.min(framing_budget);
            let before = line_budget;
            let line = read_line(&mut reader, &mut line_budget)?;
            framing_budget -= before - line_budget;
            let size = line.split(';').next().unwrap_or("");
            if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(bad_request("invalid chunk size"));
            }
            if !valid_chunk_extensions(&line[size.len()..]) {
                return Err(bad_request("invalid chunk extension"));
            }
            let n = usize::from_str_radix(size, 16).map_err(|_| too_large())?;
            if n > MAX_BODY - body.len() {
                return Err(too_large());
            }
            if n == 0 {
                let mut budget = MAX_HEAD;
                loop {
                    let line = read_line(&mut reader, &mut budget)?;
                    if line.is_empty() {
                        break;
                    }
                    let (name, _) = header(&line)?;
                    if matches!(
                        name.as_str(),
                        "content-length" | "transfer-encoding" | "host"
                    ) {
                        return Err(bad_request("forbidden framing trailer"));
                    }
                }
                break;
            }
            let old = body.len();
            body.resize(old + n, 0);
            reader.read_exact(&mut body[old..])?;
            let mut ending = [0; 2];
            reader.read_exact(&mut ending)?;
            if ending != *b"\r\n" {
                return Err(bad_request("invalid chunk ending"));
            }
        }
    } else {
        body.resize(length.unwrap_or(0), 0);
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // Hex digits are ASCII — parse from bytes, never slice `s`:
            // `&s[i+1..i+3]` panics on a `%` before a multibyte char.
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// --- Routing -------------------------------------------------------------

fn route(req: &Request, dir: &Path) -> Response {
    let path = req.path.split('?').next().unwrap_or("/");
    match (req.method.as_str(), path) {
        ("GET" | "HEAD", "/") => {
            Response::new(200, "text/html; charset=utf-8", PAGE.as_bytes().to_vec())
        }
        ("GET", "/api/scene") => Response::json(api_scene(dir)),
        ("GET", "/api/assets") => Response::json(api_assets(dir)),
        ("GET", "/api/timings") => Response::json(serde_json::json!({
            "present": dir.join("timings.json").is_file()
        })),
        ("POST", "/api/check") => {
            post_guard(req).unwrap_or_else(|| Response::json(api_check(dir, &req.body)))
        }
        ("POST", "/api/render") => {
            post_guard(req).unwrap_or_else(|| Response::json(api_render(dir, &req.body)))
        }
        ("GET" | "HEAD", p) if p.starts_with("/out/") => serve_file(dir, p, req),
        _ => Response::new(404, "text/plain", b"not found".to_vec()),
    }
}

/// POSTs must carry a JSON content type — a cross-origin page can't
/// set that without a preflight we never answer, which turns drive-by
/// CSRF into a non-issue on localhost.
fn post_guard(req: &Request) -> Option<Response> {
    let json = req
        .headers
        .iter()
        .any(|(k, v)| k == "content-type" && v.starts_with("application/json"));
    (!json).then(|| Response::new(415, "text/plain", b"expected application/json".to_vec()))
}

/// Serve `<dir>/out/<name>` with byte-range support (video scrubbing).
/// Confined twice: the name can't contain `..`/separators, and the
/// canonicalized file must stay under canonicalized `out/` (symlinks
/// pointing outside get a 404). Reads only the requested range — a
/// scrubbing `<video>` shouldn't pull the whole file per seek.
fn serve_file(dir: &Path, path: &str, req: &Request) -> Response {
    let name = url_decode(path.trim_start_matches("/out/"));
    if name.is_empty() || name.contains("..") || name.contains('/') || name.contains('\\') {
        return Response::new(403, "text/plain", b"forbidden".to_vec());
    }
    let out_root = dir.join("out").canonicalize();
    let file = dir.join("out").join(&name).canonicalize();
    let file = match (out_root, file) {
        (Ok(root), Ok(f)) if f.starts_with(&root) && f.is_file() => f,
        _ => return Response::new(404, "text/plain", b"not found".to_vec()),
    };
    let total = file.metadata().map(|m| m.len() as usize).unwrap_or(0);

    // Malformed/unsatisfiable ranges fall back to a full 200 rather
    // than risking a bad slice — the client just ignores it.
    let range = req
        .headers
        .iter()
        .find(|(k, _)| k == "range")
        .and_then(|(_, v)| parse_range(v, total));
    let (status, start, end) = match range {
        Some((s, e)) => (206, s, e.min(total)),
        None => (200, 0, total),
    };
    // Open + seek now, stream in `handle` — a 200 on a big mp4 buffers
    // nothing beyond the 64KB copy buffer.
    use std::io::{Seek, SeekFrom};
    let file = std::fs::File::open(&file)
        .and_then(|mut f| f.seek(SeekFrom::Start(start as u64)).map(|_| f));
    let Ok(file) = file else {
        return Response::new(404, "text/plain", b"not found".to_vec());
    };
    let mut res = Response {
        status,
        content_type: content_type(&name),
        body_len: (end - start) as u64,
        body: Body::File {
            file,
            remaining: (end - start) as u64,
        },
        extra_headers: Vec::new(),
    };
    res.extra_headers.push("Accept-Ranges: bytes".to_string());
    if status == 206 {
        res.extra_headers
            .push(format!("Content-Range: bytes {start}-{}/{total}", end - 1));
    }
    res
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("mp4" | "m4v" | "mov") => "video/mp4",
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

fn parse_range(v: &str, total: usize) -> Option<(usize, usize)> {
    let spec = v.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    // `end` is exclusive internally; the wire form is inclusive.
    let (start, end) = if a.is_empty() {
        // Suffix form `bytes=-N`: the last N bytes.
        let n: usize = b.parse().ok()?;
        (total.saturating_sub(n), total)
    } else {
        let start: usize = a.parse().ok()?;
        let end: usize = if b.is_empty() {
            total
        } else {
            b.parse::<usize>().ok()?.saturating_add(1)
        };
        (start, end)
    };
    (start < total && end > start).then_some((start, end))
}

// --- API -----------------------------------------------------------------

fn api_scene(dir: &Path) -> serde_json::Value {
    let file = dir.join("main.scene");
    let markup = std::fs::read_to_string(&file).unwrap_or_default();
    serde_json::json!({ "markup": markup, "path": file })
}

fn api_assets(dir: &Path) -> serde_json::Value {
    let assets = dir.join("assets");
    let mut files: Vec<String> = std::fs::read_dir(&assets)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.file_name().to_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    serde_json::json!({ "dir": assets, "files": files })
}

/// Byte offset → 1-based (line, col) for diagnostics display.
fn line_col(source: &str, byte: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in source.char_indices() {
        if i >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn diag_json(source: &str, d: &Diagnostic) -> serde_json::Value {
    let (line, col) = d.span.map(|s| line_col(source, s.start)).unwrap_or((0, 0));
    serde_json::json!({
        "severity": match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        },
        "message": d.message,
        "line": line,
        "col": col,
    })
}

fn bundle_json(bundles: &[DiagBundle]) -> serde_json::Value {
    let mut out = Vec::new();
    for b in bundles {
        for d in &b.diags {
            let mut v = diag_json(&b.source, d);
            v["file"] = serde_json::json!(b.name);
            out.push(v);
        }
    }
    serde_json::json!(out)
}

fn req_markup(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body).ok()?["markup"]
        .as_str()
        .map(String::from)
}

fn api_check(dir: &Path, body: &[u8]) -> serde_json::Value {
    let Some(markup) = req_markup(body) else {
        return serde_json::json!({ "ok": false, "error": "bad request json" });
    };
    let doc = match scene_markup::parse_document(&markup) {
        Ok(d) => d,
        Err(d) => {
            return serde_json::json!({
                "ok": false,
                "diagnostics": [diag_json(&markup, &d)],
            });
        }
    };
    let (scene, mut diags) = scene_markup::lower(&doc);
    // Same file-existence warnings `engine check` adds — the UI mustn't
    // say "clean" where the CLI warns.
    if let Some(scene) = &scene {
        diags.extend(commands::asset_warnings(scene, dir));
    }
    serde_json::json!({
        "ok": !scene_ir::has_errors(&diags),
        "diagnostics": diags.iter().map(|d| diag_json(&markup, d)).collect::<Vec<_>>(),
    })
}

fn api_render(dir: &Path, body: &[u8]) -> serde_json::Value {
    let Some(markup) = req_markup(body) else {
        return serde_json::json!({ "ok": false, "error": "bad request json" });
    };
    // Validate before touching the file — a scene that can't lower must
    // never overwrite the last-known-good main.scene on disk.
    let doc = match scene_markup::parse_document(&markup) {
        Ok(d) => d,
        Err(d) => {
            return serde_json::json!({
                "ok": false,
                "saved": false,
                "diagnostics": [diag_json(&markup, &d)],
            });
        }
    };
    let (_scene, diags) = scene_markup::lower(&doc);
    if scene_ir::has_errors(&diags) {
        return serde_json::json!({
            "ok": false,
            "saved": false,
            "diagnostics": diags.iter().map(|d| diag_json(&markup, d)).collect::<Vec<_>>(),
        });
    }
    let scene_file = dir.join("main.scene");
    if let Err(e) = std::fs::write(&scene_file, &markup) {
        return serde_json::json!({ "ok": false, "error": format!("write main.scene: {e}") });
    }
    let timings = dir.join("timings.json");
    let timings = timings.is_file().then_some(timings);
    let out = dir.join("out").join("ui.mp4");

    match commands::render_inner(&scene_file, timings.as_deref(), Some(&out), None, 4) {
        Ok(report) => serde_json::json!({
            "ok": true,
            "saved": true,
            "frames": report.frames,
            "timings": timings.is_some(),
            "url": "/out/ui.mp4",
            "diagnostics": bundle_json(&report.bundles),
        }),
        // Markup was valid, so it stays saved — the failure is
        // environmental (missing asset, ffmpeg), not authored.
        Err(err) => serde_json::json!({
            "ok": false,
            "saved": true,
            "error": err.message,
            "diagnostics": bundle_json(&err.bundles),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parses_open_and_closed() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 100)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 1000)));
        assert_eq!(parse_range("bytes=999-999", 1000), Some((999, 1000)));
        assert_eq!(parse_range("bytes=-500", 1000), Some((500, 1000))); // suffix
        assert_eq!(parse_range("bytes=-2000", 1000), Some((0, 1000))); // over-long suffix
        assert_eq!(parse_range("bytes=1000-", 1000), None); // past EOF
        assert_eq!(parse_range("items=0-9", 1000), None);
        // The crash case: inverted range must not produce start > end.
        assert_eq!(parse_range("bytes=999-100", 8336), None);
        assert_eq!(parse_range("bytes=5-5", 10), Some((5, 6))); // one byte
    }

    fn req(method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn inverted_range_serves_full_body_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("ui-range-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out").join("v.mp4"), b"0123456789").unwrap();
        let r = req("GET", "/out/v.mp4", &[("range", "bytes=9-2")], &[]);
        let res = serve_file(&dir, r.path.as_str(), &r);
        assert_eq!(res.status, 200); // bad range → full body, no panic
        assert_eq!(res.body_len, 10);
        assert!(matches!(res.body, Body::File { .. }), "file bodies stream");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn posts_without_json_content_type_are_rejected() {
        let dir = std::env::temp_dir().join(format!("ui-csrf-{}", std::process::id()));
        let r = req(
            "POST",
            "/api/check",
            &[("content-type", "text/plain")],
            br#"{"markup":"x"}"#,
        );
        let res = route(&r, &dir);
        assert_eq!(res.status, 415);
        // With the right content type the same body is served.
        let r2 = req(
            "POST",
            "/api/check",
            &[("content-type", "application/json")],
            br#"{"markup":"<scene/>"}"#,
        );
        assert_eq!(route(&r2, &dir).status, 200);
    }

    #[test]
    fn traversal_is_rejected_before_decode() {
        let dir = std::env::temp_dir();
        let r = req("GET", "/out/..%2Fsecret", &[], &[]);
        assert_eq!(serve_file(&dir, r.path.as_str(), &r).status, 403);
    }

    #[test]
    fn url_decode_handles_escapes_and_literals() {
        assert_eq!(url_decode("a%20b.mp4"), "a b.mp4");
        assert_eq!(url_decode("%2e%2e%2fx"), "../x");
        assert_eq!(url_decode("plain.mp4"), "plain.mp4");
        assert_eq!(url_decode("%zz"), "%zz"); // bad escape passes through
    }

    #[test]
    fn line_col_tracks_newlines() {
        let src = "abc\ndef\nghi";
        assert_eq!(line_col(src, 0), (1, 1));
        assert_eq!(line_col(src, 3), (1, 4));
        assert_eq!(line_col(src, 4), (2, 1));
        assert_eq!(line_col(src, 9), (3, 2));
    }

    #[test]
    fn check_reports_error_positions() {
        let dir = std::env::temp_dir();
        let body = serde_json::json!({
            "markup": "<scene canvas=\"64x64\" fps=\"30\">\n<track><bogus/></track>\n</scene>"
        });
        let res = api_check(&dir, body.to_string().as_bytes());
        assert_eq!(res["ok"], false);
        let diags = res["diagnostics"].as_array().unwrap();
        assert!(!diags.is_empty());
        assert!(diags.iter().any(|d| d["line"].as_u64() == Some(2)));
    }

    #[test]
    fn check_clean_markup_is_ok() {
        let dir = std::env::temp_dir();
        let body = serde_json::json!({
            "markup": "<scene canvas=\"64x64\" fps=\"30\"><track kind=\"visual\"><text during=\"0s..1s\">x</text></track></scene>"
        });
        let res = api_check(&dir, body.to_string().as_bytes());
        assert_eq!(res["ok"], true);
    }

    #[test]
    fn check_warns_on_missing_assets_like_the_cli() {
        let dir = std::env::temp_dir().join(format!("ui-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = serde_json::json!({
            "markup": "<scene canvas=\"64x64\" fps=\"30\"><track kind=\"visual\"><clip src=\"assets/nope.mp4\" during=\"0s..1s\"/></track></scene>"
        });
        let res = api_check(&dir, body.to_string().as_bytes());
        assert_eq!(res["ok"], true); // warning, not error
        let diags = res["diagnostics"].as_array().unwrap();
        assert!(
            diags
                .iter()
                .any(|d| d["message"].as_str().unwrap().contains("nope.mp4")),
            "missing-asset warning should reach the UI: {diags:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_with_broken_markup_leaves_main_scene_untouched() {
        let dir = std::env::temp_dir().join(format!("ui-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.scene");
        let good = "<scene canvas=\"64x64\" fps=\"30\"><track kind=\"visual\"/></scene>";
        std::fs::write(&file, good).unwrap();

        // Invalid markup → rejected, file untouched.
        let bad = serde_json::json!({ "markup": "<scene><bogus" });
        let res = api_render(&dir, bad.to_string().as_bytes());
        assert_eq!(res["ok"], false);
        assert_eq!(res["saved"], false);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), good);

        // Valid-but-error markup (missing required attrs) same story.
        let bad2 = serde_json::json!({ "markup": "<scene><track/></scene>" });
        let res2 = api_render(&dir, bad2.to_string().as_bytes());
        assert_eq!(res2["ok"], false);
        assert_eq!(res2["saved"], false);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), good);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn url_decode_never_panics_on_multibyte_after_percent() {
        // `%` followed by a multibyte char used to panic on a `&str`
        // slice at a non-char-boundary — one crafted URL killed the UI.
        assert_eq!(url_decode("%€x"), "%€x");
        assert_eq!(url_decode("/out/%日本語.mp4"), "/out/%日本語.mp4");
        assert_eq!(url_decode("%e2%82%ac"), "€"); // valid escapes still decode
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_out_is_not_served() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("ui-sym-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, b"top secret").unwrap();
        symlink(&secret, dir.join("out").join("leak.mp4")).unwrap();
        let r = req("GET", "/out/leak.mp4", &[], &[]);
        let res = serve_file(&dir, r.path.as_str(), &r);
        assert!(
            res.status == 403 || res.status == 404,
            "symlink outside out/ must not be served: {}",
            res.status
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn head_returns_headers_without_body() {
        let dir = std::env::temp_dir().join(format!("ui-head-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out").join("v.mp4"), b"0123456789").unwrap();
        let r = req("HEAD", "/out/v.mp4", &[], &[]);
        let res = route(&r, &dir);
        assert_eq!(res.status, 200);
        // Length known for Content-Length; `handle` skips the body for
        // HEAD — that's the wire contract.
        assert_eq!(res.body_len, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_type_follows_extension() {
        assert_eq!(content_type("a.mp4"), "video/mp4");
        assert_eq!(content_type("b.wav"), "audio/wav");
        assert_eq!(content_type("c.png"), "image/png");
        assert_eq!(content_type("d.bin"), "application/octet-stream");
    }
}

#[cfg(test)]
mod framing_tests {
    use super::*;
    fn parse(bytes: &[u8]) -> std::io::Result<Option<Request>> {
        read_request(&mut &bytes[..])
    }
    #[test]
    fn chunks_extensions_trailers_and_fragmentation() {
        struct Fragments<'a>(&'a [u8]);
        impl Read for Fragments<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let n = out.len().min(self.0.len()).min(1);
                out[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let bytes = b"POST /api/check HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3;name=value\r\nabc\r\n2\r\nde\r\n0\r\nX-Checksum: yes\r\n\r\n";
        let req = read_request(&mut Fragments(bytes)).unwrap().unwrap();
        assert_eq!(req.body, b"abcde");
    }
    #[test]
    fn invalid_framing_is_rejected() {
        for bytes in [
            "Transfer-Encoding: chunked\r\nContent-Length: 0\r\n\r\n0\r\n\r\n",
            "Content-Length: 1\r\nContent-Length: 1\r\n\r\nx",
            "Content-Length: invalid\r\n\r\n",
            "Content-Length: 2\r\n\r\nx",
            "Transfer-Encoding: gzip, chunked\r\n\r\n",
            "Transfer-Encoding: chunked\r\n\r\nZ\r\n",
            "Transfer-Encoding: chunked\r\n\r\n2\r\nx",
            "Transfer-Encoding: chunked\r\n\r\n1\r\nxXX0\r\n\r\n",
            "Transfer-Encoding: chunked\r\n\r\n0\r\nContent-Length: 0\r\n\r\n",
        ] {
            assert!(
                parse(format!("POST / HTTP/1.1\r\n{bytes}").as_bytes()).is_err(),
                "{bytes}"
            );
        }
    }
    #[test]
    fn oversize_and_timeout_are_errors() {
        for tail in [
            format!("Content-Length: {}\r\n\r\n", MAX_BODY + 1),
            format!("Transfer-Encoding: chunked\r\n\r\n{:x}\r\n", MAX_BODY + 1),
            format!(
                "Transfer-Encoding: chunked\r\n\r\n1;{}\r\n",
                "x".repeat(8192)
            ),
        ] {
            let error = parse(format!("POST / HTTP/1.1\r\n{tail}").as_bytes())
                .err()
                .unwrap();
            assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
        }
        struct Timeout;
        impl Read for Timeout {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::TimedOut.into())
            }
        }
        assert_eq!(
            read_request(&mut Timeout).err().unwrap().kind(),
            std::io::ErrorKind::TimedOut
        );
    }
}
