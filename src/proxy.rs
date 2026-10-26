//! Local MITM HTTPS forward proxy.
//!
//! Bun / `claude` connect here as `HTTPS_PROXY`. We:
//!   1. Accept a `CONNECT host:port HTTP/1.1` request.
//!   2. Reply `200 Connection Established` and hijack the underlying TCP.
//!   3. Mint a leaf cert for `host` signed by our local CA, then perform a
//!      TLS server handshake against the hijacked stream.
//!   4. Open a fresh TLS client connection upstream to `host:port`, validating
//!      against the OS trust store via `rustls-native-certs`.
//!   5. For each HTTP request observed in cleartext on the proxy: log it,
//!      forward upstream, observe the response (buffered for normal bodies,
//!      tee-streamed for `text/event-stream`), and write a record to
//!      `~/.pine/sessions/<id>/traffic.jsonl`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use futures_util::stream::{self, StreamExt};
use http::header::{CONTENT_TYPE, HOST};
use http::HeaderName;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1 as server_http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::server::ResolvesServerCert;
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use time::OffsetDateTime;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{debug, warn};
use ulid::Ulid;

use crate::ca::{Ca, LeafCertCache};
use crate::log::{ResponsePayload, SessionLogger, SidePayload, TrafficRecord};
use crate::sse::SseParser;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type BodyOut = BoxBody<Bytes, BoxError>;

/// Per-process proxy state shared across all incoming tunnels.
pub struct ProxyState {
    pub leaf_cache: LeafCertCache,
    pub upstream_connector: TlsConnector,
    pub logger: Arc<SessionLogger>,
    pub host_filter: HostFilter,
    pub passthrough_only_logged: bool,
}

#[derive(Clone, Default)]
pub struct HostFilter {
    /// Plain hostnames or `*.suffix` glob patterns. Matches case-insensitive.
    patterns: Vec<String>,
}

impl HostFilter {
    pub fn new(patterns: Vec<String>) -> Self {
        Self { patterns }
    }

    pub fn matches(&self, host: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        let host = host.to_ascii_lowercase();
        for p in &self.patterns {
            let p = p.to_ascii_lowercase();
            if p == "*" {
                return true;
            }
            if let Some(suffix) = p.strip_prefix("*.") {
                if host == suffix || host.ends_with(&format!(".{suffix}")) {
                    return true;
                }
            } else if host == p {
                return true;
            }
        }
        false
    }
}

pub fn build_upstream_connector() -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        // ignore individual parse failures; native bundles often contain duplicates
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(anyhow!(
            "could not load any system root certificates via rustls-native-certs"
        ));
    }
    let mut cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // ALPN: only HTTP/1.1 for v0.1. h2 is M5.
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsConnector::from(Arc::new(cfg)))
}

pub async fn bind_listener(port: u16) -> Result<TcpListener> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let listener = TcpListener::bind(addr).await?;
    Ok(listener)
}

pub async fn run(listener: TcpListener, state: Arc<ProxyState>) -> Result<()> {
    let local = listener.local_addr()?;
    debug!(%local, "pine proxy listening");
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "accept failed");
                tokio::time::sleep(StdDuration::from_millis(50)).await;
                continue;
            }
        };
        let _ = sock.set_nodelay(true);
        debug!(%peer, "tcp accepted");
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_proxy_connection(sock, state).await {
                warn!(error = %e, "proxy connection ended with error");
            }
        });
    }
}

async fn serve_proxy_connection(sock: TcpStream, state: Arc<ProxyState>) -> Result<()> {
    let io = TokioIo::new(sock);
    let svc = service_fn(move |req: Request<Incoming>| {
        let state = state.clone();
        async move { Ok::<_, Infallible>(proxy_entry(req, state).await) }
    });
    server_http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(io, svc)
        .with_upgrades()
        .await
        .map_err(|e| anyhow!("serve_connection: {e}"))?;
    Ok(())
}

async fn proxy_entry(req: Request<Incoming>, state: Arc<ProxyState>) -> Response<BodyOut> {
    if req.method() == Method::CONNECT {
        let authority = match req.uri().authority().cloned() {
            Some(a) => a,
            None => return text_response(StatusCode::BAD_REQUEST, "CONNECT without authority"),
        };
        let host = authority.host().to_string();
        let port = authority.port_u16().unwrap_or(443);
        let logged = state.host_filter.matches(&host);

        if state.passthrough_only_logged && !logged {
            return text_response(
                StatusCode::FORBIDDEN,
                "pine: passthrough_only_logged refuses non-tapped hosts",
            );
        }

        // Take the request out so we can move it into the spawned task.
        tokio::spawn(async move {
            let upgraded = match hyper::upgrade::on(req).await {
                Ok(u) => u,
                Err(e) => {
                    warn!(error = %e, "CONNECT upgrade failed");
                    return;
                }
            };
            if logged {
                if let Err(e) = mitm_tunnel(upgraded, host.clone(), port, state.clone()).await {
                    // These fire for every retry when the network is down
                    // ("downstream serve: connection error" once claude
                    // hangs up after the upstream connect fails). Keep them
                    // at debug so a flapping connection doesn't flood the
                    // user's terminal; the error is still in traffic.jsonl.
                    debug!(error = %e, host = %host, "MITM tunnel error");
                }
            } else if let Err(e) = blind_tunnel(upgraded, host.clone(), port).await {
                debug!(error = %e, host = %host, "blind tunnel error");
            }
        });

        // Empty 200 to signal the tunnel is open.
        Response::builder()
            .status(StatusCode::OK)
            .body(empty_body())
            .unwrap()
    } else {
        text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "pine only supports HTTPS via CONNECT",
        )
    }
}

/// Pass-through tunnel: copy bytes both ways without TLS termination.
async fn blind_tunnel(
    upgraded: hyper::upgrade::Upgraded,
    host: String,
    port: u16,
) -> Result<()> {
    let mut downstream = TokioIo::new(upgraded);
    let mut upstream = TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| format!("connect upstream {host}:{port}"))?;
    let _ = upstream.set_nodelay(true);
    let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
    Ok(())
}

/// MITM tunnel: terminate TLS, serve HTTP/1.1 to the client, forward to upstream.
async fn mitm_tunnel(
    upgraded: hyper::upgrade::Upgraded,
    host: String,
    port: u16,
    state: Arc<ProxyState>,
) -> Result<()> {
    let cert = state.leaf_cache.get_or_mint(&host)?;
    let server_cfg = build_server_config(cert);
    let acceptor = TlsAcceptor::from(Arc::new(server_cfg));
    let tls = acceptor
        .accept(TokioIo::new(upgraded))
        .await
        .with_context(|| format!("downstream TLS handshake for {host}"))?;
    let io = TokioIo::new(tls);

    let host_ref = host.clone();
    let svc = service_fn(move |req: Request<Incoming>| {
        let state = state.clone();
        let host = host_ref.clone();
        async move { Ok::<_, Infallible>(handle_request(req, state, host, port).await) }
    });

    // .with_upgrades() is REQUIRED for WebSocket: without it, hyper
    // happily forwards the 101 status to the client but never hands the
    // upgraded I/O to `hyper::upgrade::on(&mut req)`, so the WS log file
    // gets opened (handshake detection succeeds) and then both
    // `req_upgrade_fut.await` and `upstream_upgrade_fut.await` hang
    // forever. Result: empty .ws.jsonl. The outer server (CONNECT
    // tunnel) already has this; the inner MITM server needs it too.
    server_http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(io, svc)
        .with_upgrades()
        .await
        .map_err(|e| anyhow!("downstream serve: {e}"))?;
    Ok(())
}

fn build_server_config(ck: Arc<CertifiedKey>) -> ServerConfig {
    #[derive(Debug)]
    struct R(Arc<CertifiedKey>);
    impl ResolvesServerCert for R {
        fn resolve(&self, _hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }
    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(R(ck)));
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    cfg
}

async fn handle_request(
    req: Request<Incoming>,
    state: Arc<ProxyState>,
    host: String,
    port: u16,
) -> Response<BodyOut> {
    let req_id = Ulid::new().to_string();
    let ts_start = OffsetDateTime::now_utc();
    let logger = state.logger.clone();

    match handle_request_inner(req, state, host.clone(), port, req_id.clone(), ts_start).await {
        Ok(r) => r,
        Err(err) => {
            // Demoted to debug: this is the per-request twin of the MITM
            // tunnel error above. When upstream is unreachable, claude
            // retries aggressively and we'd otherwise emit a WARN per
            // attempt. The failure is still surfaced as a 502 to claude
            // and recorded in traffic.jsonl below.
            debug!(error = %err, host = %host, req_id = %req_id, "request handling failed");
            // Best-effort error record so the failure is visible in the log.
            let record = TrafficRecord {
                id: req_id.clone(),
                ts_start,
                ts_end: OffsetDateTime::now_utc(),
                method: "?".to_string(),
                url: format!("https://{host}:{port}/?"),
                http_version: "HTTP/1.1".to_string(),
                upstream_addr: Some(format!("{host}:{port}")),
                request: SidePayload {
                    headers: Vec::new(),
                    body_size: 0,
                    body_inline: None,
                    body_path: None,
                },
                response: ResponsePayload {
                    status: 0,
                    headers: Vec::new(),
                    body_size: 0,
                    body_inline: None,
                    body_path: None,
                    is_stream: false,
                    stream_path: None,
                    is_websocket: false,
                    ws_path: None,
                },
                error: Some(err.to_string()),
            };
            let _ = logger.append_traffic(&record).await;
            text_response(StatusCode::BAD_GATEWAY, &format!("pine upstream error: {err}"))
        }
    }
}

async fn handle_request_inner(
    req: Request<Incoming>,
    state: Arc<ProxyState>,
    host: String,
    port: u16,
    req_id: String,
    ts_start: OffsetDateTime,
) -> Result<Response<BodyOut>> {
    let logger = state.logger.clone();

    let mut req = req;
    let is_ws_handshake = req.method() == Method::GET
        && req.headers().get(http::header::CONNECTION).map(|v| v.to_str().unwrap_or("").to_ascii_lowercase().contains("upgrade")).unwrap_or(false)
        && req.headers().get(http::header::UPGRADE).map(|v| v.to_str().unwrap_or("").to_ascii_lowercase() == "websocket").unwrap_or(false)
        && req.headers().contains_key("sec-websocket-key")
        && req.headers().get("sec-websocket-version").map(|v| v.as_bytes() == b"13").unwrap_or(false);

    let mut req_upgrade = if is_ws_handshake {
        Some(hyper::upgrade::on(&mut req))
    } else {
        None
    };

    let (mut parts, body) = req.into_parts();
    let req_method = parts.method.clone();
    let req_uri = parts.uri.clone();
    let path_and_query = req_uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let url = format!("https://{host}{path_and_query}");
    let http_version_str = format!("{:?}", parts.version);

    // Ensure Host header is set for HTTP/1.1 upstream.
    if !parts.headers.contains_key(HOST) {
        if let Ok(v) = http::HeaderValue::from_str(&format!("{host}")) {
            parts.headers.insert(HOST, v);
        }
    }
    // Strip hop-by-hop headers. The proxy is not transparent, so we don't want
    // to forward `proxy-connection` etc.
    strip_hop_by_hop(&mut parts.headers, is_ws_handshake);

    let req_headers_capture = capture_headers(&parts.headers);

    // Force upstream responses to be uncompressed.
    //
    // This is done AFTER `capture_headers` above so the recorded request
    // still shows the *client's* original `accept-encoding` (e.g. claude's
    // `gzip, deflate, br, zstd`) — useful for debugging the client.
    //
    // Why we override it on the wire: Anthropic's `text/event-stream`
    // responses are returned with `content-encoding: gzip` whenever the
    // client advertises gzip support. The previous code fed those gzip
    // bytes straight into `SseParser`, which scans for the `\n\n` event
    // delimiter. Compressed bytes virtually never contain that pattern,
    // so essentially zero events were parsed; the only thing the parser
    // ever emitted was the occasional spurious empty event whenever a
    // random byte run happened to look like `data:\n\n`. That's why
    // `stream/<id>.sse.jsonl` files for `/v1/messages` were ending up
    // with one stray `{"event":null,"data":""}` line and nothing else,
    // even though the wire carried thousands of real SSE bytes.
    //
    // pine is a debugging proxy on localhost — wire compression
    // saves nothing here and only hurts inspection. Forcing `identity`
    // upstream gives us plaintext SSE we can actually parse, plaintext
    // JSON bodies in `bodies/`, and the TUI continues to work because
    // its body decoder already treats unencoded payloads as the common
    // case (see `packages/tui/decoder.py`).
    //
    // Skipped for the WebSocket handshake — `accept-encoding` is
    // meaningless there and we don't want to perturb the upgrade.
    if !is_ws_handshake {
        parts.headers.insert(
            http::header::ACCEPT_ENCODING,
            http::HeaderValue::from_static("identity"),
        );
    }

    // Buffer the request body. For Anthropic API requests these are small JSON
    // payloads; buffering is fine and lets us log the exact bytes.
    let req_body_bytes: Bytes = body
        .collect()
        .await
        .map_err(|e| anyhow!("reading downstream request body: {e}"))?
        .to_bytes();

    let (req_body_size, req_body_path, req_body_inline) =
        logger.store_request_body(&req_id, &req_body_bytes).await?;

    // Best-effort: note which Claude Code conversation this request is on.
    logger.observe_request(&parts.headers, &req_body_bytes).await;

    // Connect upstream.
    let upstream_tcp = TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| format!("connect upstream {host}:{port}"))?;
    let _ = upstream_tcp.set_nodelay(true);
    let server_name = ServerName::try_from(host.clone())
        .map_err(|e| anyhow!("invalid SNI hostname {host}: {e}"))?;
    let upstream_tls = state
        .upstream_connector
        .connect(server_name, upstream_tcp)
        .await
        .with_context(|| format!("upstream TLS handshake to {host}"))?;
    let upstream_io = TokioIo::new(upstream_tls);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(upstream_io)
        .await
        .with_context(|| "upstream HTTP/1.1 handshake")?;
    // Mirror the server-side .with_upgrades(): the upstream connection
    // driver also needs this to surrender its I/O to
    // `hyper::upgrade::on(&mut upstream_resp)` after a 101. Without it,
    // the upstream upgrade future never resolves and `proxy_ws_stream`
    // never starts on the s2c half — so even if the downstream client
    // sends frames they have nowhere to go.
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            debug!(error = %e, "upstream connection closed");
        }
    });

    let upstream_req = Request::from_parts(parts, Full::new(req_body_bytes.clone()));
    let mut upstream_resp = sender
        .send_request(upstream_req)
        .await
        .with_context(|| "send_request upstream")?;

    let is_ws_101 = is_ws_handshake && upstream_resp.status() == StatusCode::SWITCHING_PROTOCOLS;
    let mut upstream_upgrade = if is_ws_101 {
        Some(hyper::upgrade::on(&mut upstream_resp))
    } else {
        None
    };

    let (resp_parts, resp_body) = upstream_resp.into_parts();
    let resp_status = resp_parts.status.as_u16();

    if is_ws_101 {
        let mut resp_parts = resp_parts;
        strip_hop_by_hop(&mut resp_parts.headers, true);

        let logger_for_record = logger.clone();
        let req_id_for_record = req_id.clone();
        let method_for_record = req_method.as_str().to_string();
        let url_for_record = url.clone();
        let http_version_for_record = http_version_str.clone();
        let upstream_addr = format!("{host}:{port}");
        let req_headers_capture_owned = req_headers_capture.clone();
        let resp_headers_capture = capture_headers(&resp_parts.headers);
        let resp_headers_capture_owned = resp_headers_capture.clone();
        let req_body_size_owned = req_body_size;
        let req_body_inline_owned = req_body_inline.clone();
        let req_body_path_owned = req_body_path.clone();

        let req_upgrade_fut = req_upgrade.take().unwrap();
        let upstream_upgrade_fut = upstream_upgrade.take().unwrap();

        let (ws_rel_path, ws_file) = match logger_for_record.open_ws_log(&req_id_for_record).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "failed to open ws log file");
                return Ok(Response::from_parts(resp_parts, empty_body()));
            }
        };
        let ws_file = std::sync::Arc::new(tokio::sync::Mutex::new(ws_file));

        tokio::spawn(async move {
            let down = match req_upgrade_fut.await {
                Ok(u) => u,
                Err(e) => {
                    warn!(error = %e, "downstream WS upgrade failed");
                    return;
                }
            };
            let up = match upstream_upgrade_fut.await {
                Ok(u) => u,
                Err(e) => {
                    warn!(error = %e, "upstream WS upgrade failed");
                    return;
                }
            };

            let down_io = hyper_util::rt::TokioIo::new(down);
            let up_io = hyper_util::rt::TokioIo::new(up);

            let (down_rx, down_tx) = tokio::io::split(down_io);
            let (up_rx, up_tx) = tokio::io::split(up_io);

            let logger_up = ws_file.clone();
            let logger_down = ws_file.clone();

            let up_task = tokio::spawn(async move {
                let _ = crate::ws::proxy_ws_stream(down_rx, up_tx, "c2s", logger_up).await;
            });

            let down_task = tokio::spawn(async move {
                let _ = crate::ws::proxy_ws_stream(up_rx, down_tx, "s2c", logger_down).await;
            });

            let _ = tokio::join!(up_task, down_task);

            let record = TrafficRecord {
                id: req_id_for_record,
                ts_start,
                ts_end: OffsetDateTime::now_utc(),
                method: method_for_record,
                url: url_for_record,
                http_version: http_version_for_record,
                upstream_addr: Some(upstream_addr),
                request: SidePayload {
                    headers: logger_for_record.redact_headers(&req_headers_capture_owned),
                    body_size: req_body_size_owned,
                    body_inline: req_body_inline_owned,
                    body_path: req_body_path_owned,
                },
                response: ResponsePayload {
                    status: resp_status,
                    headers: logger_for_record.redact_headers(&resp_headers_capture_owned),
                    body_size: 0,
                    body_inline: None,
                    body_path: None,
                    is_stream: false,
                    stream_path: None,
                    is_websocket: true,
                    ws_path: Some(ws_rel_path),
                },
                error: None,
            };
            if let Err(e) = logger_for_record.append_traffic(&record).await {
                warn!(error = %e, "writing WS traffic record");
            }
        });

        return Ok(Response::from_parts(resp_parts, empty_body()));
    }

    let resp_headers_capture = capture_headers(&resp_parts.headers);
    let is_sse = resp_parts
        .headers
        .get(CONTENT_TYPE)
        .map(|v| v.as_bytes().starts_with(b"text/event-stream"))
        .unwrap_or(false);

    if is_sse {
        // Stream-tee: forward frames as they arrive, append parsed events to
        // stream/<id>.sse.jsonl, then write the traffic record on EOF.
        let (stream_rel, stream_file) = logger.open_stream_log(&req_id).await?;
        let stream_file = Arc::new(Mutex::new(stream_file));
        let parser = Arc::new(Mutex::new(SseParser::new()));

        let req_headers_capture_owned = req_headers_capture.clone();
        let resp_headers_capture_owned = resp_headers_capture.clone();
        let req_id_for_stream = req_id.clone();
        let stream_rel_for_record = stream_rel.clone();
        let logger_for_record = logger.clone();
        let host_for_record = host.clone();
        let url_for_record = url.clone();
        let method_for_record = req_method.as_str().to_string();
        let http_version_for_record = http_version_str.clone();
        let upstream_addr = format!("{host}:{port}");

        // Track cumulative bytes seen so the final record reports a useful
        // body_size for the SSE stream.
        let bytes_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let bytes_seen_for_finish = bytes_seen.clone();

        let st = stream::unfold(
            (resp_body, parser.clone(), stream_file.clone(), bytes_seen.clone()),
            |(mut body, parser, sf, bytes_seen)| async move {
                match body.frame().await {
                    None => None,
                    Some(Ok(frame)) => {
                        if let Some(data) = frame.data_ref() {
                            bytes_seen
                                .fetch_add(data.len(), std::sync::atomic::Ordering::Relaxed);
                            let events = {
                                let mut p = parser.lock().await;
                                p.feed(data)
                            };
                            if !events.is_empty() {
                                let mut f = sf.lock().await;
                                for ev in events {
                                    if let Err(e) =
                                        SessionLogger::append_sse_event(&mut *f, &ev).await
                                    {
                                        warn!(error = %e, "writing SSE event");
                                    }
                                }
                            }
                        }
                        Some((Ok::<_, BoxError>(frame), (body, parser, sf, bytes_seen)))
                    }
                    Some(Err(e)) => Some((
                        Err(Box::new(e) as BoxError),
                        (body, parser, sf, bytes_seen),
                    )),
                }
            },
        );

        // Schedule a finalization task. The unfold above doesn't have a hook
        // for "done", so we drive finalization off a oneshot dropped when the
        // body finishes. Easier: spawn a follow-up task from a wrapper stream
        // that fires when the inner completes.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let mut done_tx = Some(done_tx);
        let st = st.chain(stream::once(async move {
            // Flush trailing partial event, if any.
            let leftover = {
                let mut p = parser.lock().await;
                p.flush_remaining()
            };
            if let Some(ev) = leftover {
                let mut f = stream_file.lock().await;
                let _ = SessionLogger::append_sse_event(&mut *f, &ev).await;
            }
            if let Some(tx) = done_tx.take() {
                let _ = tx.send(());
            }
            // Yield a sentinel "no more frames" by emitting nothing —
            // but unfold has already returned None by the time chain runs.
            // We satisfy the type by returning a never-yielding pending frame
            // wrapped in Err once. To stay sound, just return an empty frame.
            Ok::<_, BoxError>(Frame::data(Bytes::new()))
        }));

        let new_body: BodyOut = BodyExt::boxed(StreamBody::new(st));

        tokio::spawn(async move {
            let _ = done_rx.await;
            let total = bytes_seen_for_finish.load(std::sync::atomic::Ordering::Relaxed);
            let record = TrafficRecord {
                id: req_id_for_stream,
                ts_start,
                ts_end: OffsetDateTime::now_utc(),
                method: method_for_record,
                url: url_for_record,
                http_version: http_version_for_record,
                upstream_addr: Some(upstream_addr),
                request: SidePayload {
                    headers: logger_for_record.redact_headers(&req_headers_capture_owned),
                    body_size: req_body_size,
                    body_inline: req_body_inline,
                    body_path: req_body_path,
                },
                response: ResponsePayload {
                    status: resp_status,
                    headers: logger_for_record.redact_headers(&resp_headers_capture_owned),
                    body_size: total,
                    body_inline: None,
                    body_path: None,
                    is_stream: true,
                    stream_path: Some(stream_rel_for_record),
                    is_websocket: false,
                    ws_path: None,
                },
                error: None,
            };
            if let Err(e) = logger_for_record.append_traffic(&record).await {
                warn!(error = %e, "writing traffic record (sse)");
            }
            let _ = host_for_record;
        });

        let resp = Response::from_parts(resp_parts, new_body);
        Ok(resp)
    } else {
        // Generic stream-tee for all non-SSE responses.
        //
        // The OLD code called `resp_body.collect().await` here, which held
        // every byte until upstream EOF before sending a single byte to the
        // client. That broke streaming for any chunked response that isn't
        // `text/event-stream` — notably Windsurf / Cognition APIs, which
        // stream tokens as chunked JSON, gRPC-Web, or Connect. Users saw the
        // entire response materialise at once instead of streaming.
        //
        // Now we forward each frame as it arrives, while also accumulating
        // the body in memory so the post-EOF log record still has the full
        // payload. Bounded-size response bodies (API completions) make the
        // memory cost acceptable; if this ever needs to support multi-GB
        // downloads we should switch to `append_response_chunk` for on-disk
        // streaming.
        let mut resp_parts = resp_parts;
        // Drop framing headers from upstream — hyper will re-emit
        // `Transfer-Encoding: chunked` (or honor `Content-Length`) itself
        // based on the body we hand it. Leaving the original headers can
        // double up `Transfer-Encoding` and break some HTTP/1.1 clients.
        strip_hop_by_hop(&mut resp_parts.headers, false);

        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let mut done_tx = Some(done_tx);
        let buf_for_finalize = buf.clone();

        let st = stream::unfold(
            (resp_body, buf.clone()),
            |(mut body, buf)| async move {
                match body.frame().await {
                    None => None,
                    Some(Ok(frame)) => {
                        if let Some(data) = frame.data_ref() {
                            let mut b = buf.lock().await;
                            b.extend_from_slice(data);
                        }
                        Some((Ok::<_, BoxError>(frame), (body, buf)))
                    }
                    Some(Err(e)) => Some((Err(Box::new(e) as BoxError), (body, buf))),
                }
            },
        );

        let st = st.chain(stream::once(async move {
            if let Some(tx) = done_tx.take() {
                let _ = tx.send(());
            }
            Ok::<_, BoxError>(Frame::data(Bytes::new()))
        }));

        let new_body: BodyOut = BodyExt::boxed(StreamBody::new(st));

        let logger_for_record = logger.clone();
        let req_id_for_record = req_id.clone();
        let method_for_record = req_method.as_str().to_string();
        let url_for_record = url.clone();
        let http_version_for_record = http_version_str.clone();
        let upstream_addr = format!("{host}:{port}");
        let req_headers_capture_owned = req_headers_capture.clone();
        let resp_headers_capture_owned = resp_headers_capture.clone();
        let req_body_size_owned = req_body_size;
        let req_body_inline_owned = req_body_inline.clone();
        let req_body_path_owned = req_body_path.clone();
        let host_for_record = host.clone();

        tokio::spawn(async move {
            let _ = done_rx.await;
            let bytes = {
                let mut b = buf_for_finalize.lock().await;
                std::mem::take(&mut *b)
            };
            let collected = Bytes::from(bytes);
            let (resp_body_size, resp_body_path, resp_body_inline) =
                match logger_for_record
                    .store_response_body(&req_id_for_record, &collected)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "storing streamed response body");
                        (collected.len(), None, None)
                    }
                };
            let record = TrafficRecord {
                id: req_id_for_record,
                ts_start,
                ts_end: OffsetDateTime::now_utc(),
                method: method_for_record,
                url: url_for_record,
                http_version: http_version_for_record,
                upstream_addr: Some(upstream_addr),
                request: SidePayload {
                    headers: logger_for_record.redact_headers(&req_headers_capture_owned),
                    body_size: req_body_size_owned,
                    body_inline: req_body_inline_owned,
                    body_path: req_body_path_owned,
                },
                response: ResponsePayload {
                    status: resp_status,
                    headers: logger_for_record.redact_headers(&resp_headers_capture_owned),
                    body_size: resp_body_size,
                    body_inline: resp_body_inline,
                    body_path: resp_body_path,
                    is_stream: false,
                    stream_path: None,
                    is_websocket: false,
                    ws_path: None,
                },
                error: None,
            };
            if let Err(e) = logger_for_record.append_traffic(&record).await {
                warn!(error = %e, "writing traffic record (streamed)");
            }
            let _ = host_for_record;
        });

        let resp = Response::from_parts(resp_parts, new_body);
        Ok(resp)
    }
}

fn capture_headers(headers: &http::HeaderMap) -> Vec<(String, Vec<u8>)> {
    headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
        .collect()
}

fn strip_hop_by_hop(headers: &mut http::HeaderMap, is_ws_handshake: bool) {
    let mut hop = vec![
        "connection",
        "proxy-connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "proxy-authenticate",
        "proxy-authorization",
    ];
    if is_ws_handshake {
        hop.retain(|&h| h != "connection" && h != "upgrade");
    }
    for h in hop {
        if let Ok(name) = HeaderName::from_bytes(h.as_bytes()) {
            headers.remove(&name);
        }
    }
}

fn empty_body() -> BodyOut {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full_body(b: Bytes) -> BodyOut {
    Full::new(b).map_err(|never| match never {}).boxed()
}

fn text_response(status: StatusCode, msg: &str) -> Response<BodyOut> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full_body(Bytes::from(msg.to_string())))
        .unwrap()
}

/// Convenience: load the CA, build state, and bind the listener.
pub async fn build_state(
    ca: Arc<Ca>,
    logger: Arc<SessionLogger>,
    host_filter: HostFilter,
    passthrough_only_logged: bool,
) -> Result<Arc<ProxyState>> {
    let leaf_cache = LeafCertCache::new(ca);
    let upstream_connector = build_upstream_connector()?;
    Ok(Arc::new(ProxyState {
        leaf_cache,
        upstream_connector,
        logger,
        host_filter,
        passthrough_only_logged,
    }))
}
