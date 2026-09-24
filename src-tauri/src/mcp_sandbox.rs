//! `aa-sandbox` scheme: the MCP Apps sandbox proxy, ported from the web app's
//! `/mcp-app-sandbox` route.
//!
//! An MCP App is third-party HTML, so it must not share an origin with the main
//! window (which holds the session token). The scheme gives it one:
//! `aa-sandbox://localhost` on macOS/Linux, `http://aa-sandbox.localhost` on
//! Windows. The page served here is a thin proxy: it relays JSON-RPC between the
//! host and an inner frame that receives the app's HTML, under a CSP built from
//! the app's declared domains.

use serde_json::Value;
use tauri::http::{Request, Response, StatusCode};
use tauri::{Runtime, UriSchemeContext};

use crate::web;

pub const SCHEME: &str = "aa-sandbox";
const SANDBOX_PATH: &str = "/mcp-app-sandbox";

pub fn handle<R: Runtime>(ctx: UriSchemeContext<'_, R>, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    respond(&web::app_origins(ctx.app_handle()), &request)
}

fn respond(allowed: &[String], request: &Request<Vec<u8>>) -> Response<Vec<u8>> {
    if request.uri().path() != SANDBOX_PATH {
        return web::error(StatusCode::NOT_FOUND, "not found");
    }
    let query = request.uri().query();

    // The host names its own origin; it must be one the main window can have.
    // (WebKit sends no Referer from `tauri://` pages, so it can't be derived.)
    let host = match web::query_param(query, "host") {
        Some(h) if allowed.contains(&h) => h,
        _ => return web::error(StatusCode::FORBIDDEN, "MCP Apps sandbox embedding origin is not allowed"),
    };
    if let Some(referer) = request.headers().get("referer").and_then(|v| v.to_str().ok()) {
        if web::origin_of(referer).as_deref() != Some(host.as_str()) {
            return web::error(StatusCode::FORBIDDEN, "MCP Apps sandbox embedding origin is not allowed");
        }
    }

    let csp = match web::query_param(query, "csp") {
        None => None,
        Some(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(v @ Value::Object(_)) => Some(v),
            _ => return web::error(StatusCode::BAD_REQUEST, "csp must be valid JSON"),
        },
    };

    Response::builder()
        .header("Content-Type", "text/html; charset=utf-8")
        .header("Cache-Control", "no-store")
        .header("Content-Security-Policy", build_sandbox_csp(csp.as_ref(), &host))
        .body(sandbox_html(&host).into_bytes())
        .expect("static response")
}

/// Domain declarations are data, not directives: values that could end a
/// directive or smuggle in a CSP keyword are dropped.
fn safe_source(d: &str) -> bool {
    !d.is_empty() && !d.chars().any(|c| c.is_whitespace() || ";'\"`".contains(c))
}

fn domains(csp: Option<&Value>, key: &str) -> Vec<String> {
    csp.and_then(|c| c.get(key))
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).filter(|d| safe_source(d)).map(str::to_string).collect())
        .unwrap_or_default()
}

fn directive(name: &str, fixed: &[&str], extra: &[String]) -> String {
    let mut parts = vec![name.to_string()];
    parts.extend(fixed.iter().map(|s| s.to_string()));
    parts.extend(extra.iter().cloned());
    parts.join(" ")
}

/// The HTTP CSP of the sandbox page (inherited by the app's inner frame).
/// Port of the web app's `buildSandboxCsp`.
pub fn build_sandbox_csp(csp: Option<&Value>, app_origin: &str) -> String {
    let resources = domains(csp, "resourceDomains");
    let connect = domains(csp, "connectDomains");
    let frames = domains(csp, "frameDomains");
    let base_uris = domains(csp, "baseUriDomains");
    let ancestor = if safe_source(app_origin) { app_origin } else { "'none'" };

    [
        "default-src 'none'".to_string(),
        // The app already runs arbitrary inline script in its own origin, so
        // eval grants nothing new; template-compiling view libraries need it.
        directive("script-src", &["'self'", "'unsafe-inline'", "'unsafe-eval'"], &resources),
        directive("style-src", &["'self'", "'unsafe-inline'"], &resources),
        directive("img-src", &["'self'", "data:"], &resources),
        directive("media-src", &["'self'", "data:"], &resources),
        directive("font-src", &["'self'", "data:"], &resources),
        if connect.is_empty() { "connect-src 'none'".into() } else { directive("connect-src", &[], &connect) },
        directive("worker-src", &["'self'", "blob:"], &resources),
        if frames.is_empty() { "frame-src 'none'".into() } else { directive("frame-src", &[], &frames) },
        if base_uris.is_empty() { "base-uri 'self'".into() } else { directive("base-uri", &[], &base_uris) },
        format!("frame-ancestors {ancestor}"),
    ]
    .join("; ")
}

fn sandbox_html(host_origin: &str) -> String {
    // JSON is a valid JS literal; escaping `<` keeps `</script>` out of it.
    let allowed = serde_json::to_string(host_origin).expect("string").replace('<', "\\u003c");
    SANDBOX_TEMPLATE.replace("__ALLOWED_HOST_ORIGIN__", &allowed)
}

const SANDBOX_TEMPLATE: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>MCP App sandbox</title>
    <style>
      :root, body { width: 100%; height: 100%; margin: 0; padding: 0; }
      body { overflow: hidden; }
      iframe { display: block; width: 100%; height: 100%; border: 0; }
    </style>
  </head>
  <body>
    <script>
      (() => {
        "use strict";

        if (window.self === window.top) {
          throw new Error("The MCP Apps sandbox must be embedded");
        }

        const hostOrigin = __ALLOWED_HOST_ORIGIN__;
        // WebKit omits the referrer for non-http embedders (tauri://), so it is
        // checked only when present; frame-ancestors enforces the embedder.
        const referrer = document.referrer;
        if (referrer) {
          const match = /^[a-z][a-z0-9+.-]*:\/\/[^/?#]+/i.exec(referrer);
          if (!match || match[0].toLowerCase() !== hostOrigin.toLowerCase()) {
            throw new Error("The MCP Apps sandbox embedding origin is not allowed");
          }
        }

        // A sandbox that can read its parent is misconfigured and must not run.
        try {
          void window.top.document;
          throw new Error("The MCP Apps sandbox can reach its host");
        } catch (error) {
          if (error instanceof Error && error.message === "The MCP Apps sandbox can reach its host") {
            throw error;
          }
        }

        const ownOrigin = window.location.origin;
        const inner = document.createElement("iframe");
        inner.setAttribute("sandbox", "allow-scripts allow-same-origin allow-forms");
        document.body.append(inner);

        const resourceReady = "ui/notifications/sandbox-resource-ready";
        const proxyReady = "ui/notifications/sandbox-proxy-ready";

        window.addEventListener("message", (event) => {
          if (event.source === window.parent) {
            if (event.origin !== hostOrigin) return;
            const message = event.data;
            if (!message || typeof message !== "object") return;

            if (message.method === resourceReady) {
              const params = message.params;
              if (!params || typeof params.html !== "string") return;
              if (typeof params.sandbox === "string") {
                inner.setAttribute("sandbox", params.sandbox);
              }
              const doc = inner.contentDocument;
              if (doc) {
                doc.open();
                doc.write(params.html);
                doc.close();
              } else {
                inner.srcdoc = params.html;
              }
              return;
            }

            inner.contentWindow?.postMessage(message, ownOrigin);
            return;
          }

          if (event.source === inner.contentWindow) {
            if (event.origin !== ownOrigin) return;
            window.parent.postMessage(event.data, hostOrigin);
          }
        });

        window.parent.postMessage(
          { jsonrpc: "2.0", method: proxyReady, params: {} },
          hostOrigin
        );
      })();
    </script>
  </body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn get(uri: &str, referer: Option<&str>) -> Response<Vec<u8>> {
        let mut req = Request::builder().uri(uri);
        if let Some(r) = referer {
            req = req.header("referer", r);
        }
        let allowed = vec!["tauri://localhost".to_string(), "http://localhost:1420".to_string()];
        respond(&allowed, &req.body(Vec::new()).unwrap())
    }

    #[test]
    fn csp_drops_injected_domains_and_pins_the_ancestor() {
        let csp = serde_json::json!({
            "connectDomains": ["https://api.example.com", "https://x.com; script-src *", "'unsafe-inline'"],
            "resourceDomains": ["https://cdn.example.com"],
        });
        let header = build_sandbox_csp(Some(&csp), "tauri://localhost");
        assert!(header.contains("connect-src https://api.example.com;"));
        assert!(!header.contains("x.com"));
        assert!(header.contains("script-src 'self' 'unsafe-inline' 'unsafe-eval' https://cdn.example.com;"));
        assert!(header.contains("frame-src 'none'"));
        assert!(header.ends_with("frame-ancestors tauri://localhost"));
    }

    #[test]
    fn csp_without_declarations_is_closed() {
        let header = build_sandbox_csp(None, "http://localhost:1420");
        assert!(header.contains("connect-src 'none'"));
        assert!(header.contains("base-uri 'self'"));
        assert!(header.ends_with("frame-ancestors http://localhost:1420"));
    }

    #[test]
    fn serves_only_allowed_hosts() {
        let ok = get("aa-sandbox://localhost/mcp-app-sandbox?host=tauri%3A%2F%2Flocalhost", None);
        assert_eq!(ok.status(), StatusCode::OK);
        assert!(String::from_utf8_lossy(ok.body()).contains(r#"const hostOrigin = "tauri://localhost";"#));

        let bad = get("aa-sandbox://localhost/mcp-app-sandbox?host=https%3A%2F%2Fevil.com", None);
        assert_eq!(bad.status(), StatusCode::FORBIDDEN);
        let spoofed = get(
            "aa-sandbox://localhost/mcp-app-sandbox?host=tauri%3A%2F%2Flocalhost",
            Some("https://evil.com/page"),
        );
        assert_eq!(spoofed.status(), StatusCode::FORBIDDEN);
        let dev = get(
            "aa-sandbox://localhost/mcp-app-sandbox?host=http%3A%2F%2Flocalhost%3A1420",
            Some("http://localhost:1420/"),
        );
        assert_eq!(dev.status(), StatusCode::OK);
        assert_eq!(get("aa-sandbox://localhost/other", None).status(), StatusCode::NOT_FOUND);
        let bad_csp = get("aa-sandbox://localhost/mcp-app-sandbox?host=tauri%3A%2F%2Flocalhost&csp=%5B%5D", None);
        assert_eq!(bad_csp.status(), StatusCode::BAD_REQUEST);
    }
}
