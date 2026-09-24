//! Small helpers shared by the app's custom URI schemes (`aa-sandbox`,
//! `aa-plugin`): which origins the main window may have, and URL decoding.

use tauri::http::{Response, StatusCode};
use tauri::{AppHandle, Runtime, Url};

/// `scheme://host[:port]` of a URL. Computed by hand because `Url::origin()`
/// is opaque ("null") for non-special schemes like `tauri://`.
pub fn origin_of(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// Origins the main window can have, i.e. the only ones allowed to embed our
/// schemes. macOS/Linux serve the bundled app from `tauri://localhost`,
/// Windows from `http(s)://tauri.localhost`; debug builds load `devUrl`.
pub fn app_origins<R: Runtime>(app: &AppHandle<R>) -> Vec<String> {
    let mut origins = vec![
        "tauri://localhost".to_string(),
        "http://tauri.localhost".to_string(),
        "https://tauri.localhost".to_string(),
    ];
    if cfg!(debug_assertions) {
        if let Some(dev) = app.config().build.dev_url.as_ref().and_then(|u| origin_of(u.as_str())) {
            origins.push(dev);
        }
    }
    origins
}

/// Decode `%XX` escapes (and `+` as space when `plus_is_space`, for query
/// strings). Invalid escapes are an error rather than passed through, so a
/// path guard never sees a half-decoded segment.
pub fn percent_decode(s: &str, plus_is_space: bool) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// First value of `name` in a raw query string.
pub fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (percent_decode(key, true)? == name).then(|| percent_decode(value, true)).flatten()
    })
}

/// A plain-text error response.
pub fn error(status: StatusCode, message: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("Cache-Control", "no-store")
        .body(message.as_bytes().to_vec())
        .expect("static response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_keeps_custom_schemes_and_ports() {
        assert_eq!(origin_of("tauri://localhost/index.html").as_deref(), Some("tauri://localhost"));
        assert_eq!(origin_of("http://localhost:1420/x?y").as_deref(), Some("http://localhost:1420"));
        assert_eq!(origin_of("not a url"), None);
    }

    #[test]
    fn decodes_escapes_and_rejects_bad_ones() {
        assert_eq!(percent_decode("a%2Fb", false).as_deref(), Some("a/b"));
        assert_eq!(percent_decode("a+b", true).as_deref(), Some("a b"));
        assert_eq!(percent_decode("a+b", false).as_deref(), Some("a+b"));
        assert_eq!(percent_decode("%zz", false), None);
        assert_eq!(percent_decode("%2", false), None);
    }

    #[test]
    fn reads_query_params() {
        let q = Some("csp=%7B%22a%22%3A1%7D&host=tauri%3A%2F%2Flocalhost");
        assert_eq!(query_param(q, "csp").as_deref(), Some("{\"a\":1}"));
        assert_eq!(query_param(q, "host").as_deref(), Some("tauri://localhost"));
        assert_eq!(query_param(q, "missing"), None);
    }
}
