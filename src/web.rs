//! `corral web`: the web front for Corral's own HTTPS name (the `name` setting).
//!
//! - Port 80 redirects to HTTPS; port 443 terminates TLS and forwards to the gate. While the gate is closed
//!   (Corral is off), it serves a page saying so, with live gauges and an **Open the gate** button.
//! - Renames llama.cpp's web app from "llama-ui" to "Corral" (the tab title and the install name), and keeps the
//!   app from installing its service worker, whose cache would answer reloads instead of the server.
//! - Sockets come from systemd; without systemd it listens on 127.0.0.1:18480 and 18443 for testing.
//! - Exits after 5 minutes with no request in progress; systemd starts it again on the next visit. An open chat
//!   keeps a live stream to the router, which counts as a request.
//!
//! Certificate: `fullchain.pem` and `privkey.pem` in Corral's `tls` folder.

mod panel;

use std::io::Read;
use std::net::SocketAddr;
use std::os::fd::FromRawFd;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::{Bytes, Incoming};
use hyper::header::{CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HOST, LOCATION};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::{Config, config_dir};

const IDLE_EXIT: Duration = Duration::from_secs(300);
const TEST_HTTP: &str = "127.0.0.1:18480";
const TEST_HTTPS: &str = "127.0.0.1:18443";
const OFF_PAGE: &str = include_str!("../assets/off.html");
/// llama.cpp's own icon, served while Corral is off so the tab looks the same either way.
const FAVICON: &str = include_str!("../assets/favicon.svg");
/// Served at /sw.js in place of the web app's service worker: removes an installed one.
const NO_WORKER: &str = include_str!("../assets/no-worker.js");

/// Added to the web app's <head>, before its own code runs:
/// - The app builds its tab title as "<chat> - llama-ui" inside its script bundle and puts the name in a <meta> for
///   home-screen installs. Rather than rewrite the 8 MB bundle, a watcher swaps the name wherever it appears.
/// - Service worker registration is turned off, so the browser never caches the app behind the server's back.
const HEAD_FIX: &str = r#"<title>Corral</title><script>(()=>{if(navigator.serviceWorker)navigator.serviceWorker.register=()=>Promise.reject(new Error("Corral runs without a service worker"));const fix=()=>{const t=document.title,n=t.replace(/llama-ui$/,"Corral");if(n!==t)document.title=n;for(const m of document.head.querySelectorAll('meta[content="llama-ui"]'))m.content="Corral"};new MutationObserver(fix).observe(document.head,{subtree:true,childList:true,characterData:true,attributes:true,attributeFilter:["content"]});fix()})()</script>"#;

type Body = BoxBody<Bytes, hyper::Error>;

/// Where requests are forwarded: the gate on this machine.
static GATE: OnceLock<SocketAddr> = OnceLock::new();
/// Pages allowed to open the gate: this web front's own name.
static ORIGINS: OnceLock<Vec<String>> = OnceLock::new();

/// Requests in progress and the time of the last activity, for the idle exit. A connection the browser merely keeps
/// open for later doesn't count; a streamed response counts until its last byte.
static BUSY: AtomicUsize = AtomicUsize::new(0);
static LAST_ACTIVE: AtomicU64 = AtomicU64::new(0);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

struct Busy;

impl Busy {
    fn start() -> Self {
        BUSY.fetch_add(1, Ordering::SeqCst);
        LAST_ACTIVE.store(now(), Ordering::SeqCst);
        Busy
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.fetch_sub(1, Ordering::SeqCst);
        LAST_ACTIVE.store(now(), Ordering::SeqCst);
    }
}

fn full(body: impl Into<Bytes>) -> Body {
    Full::new(body.into()).map_err(|never| match never {}).boxed()
}

/// A response Corral makes itself, never cached by the browser.
fn own(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header(CACHE_CONTROL, "no-store")
        .body(full(body))
        .expect("valid response")
}

pub fn run() -> ExitCode {
    let config = Config::load();
    let _ = GATE.set(SocketAddr::from(([127, 0, 0, 1], config.gate_port())));
    let origins = config
        .name()
        .map(|name| vec![format!("https://{name}"), format!("https://{name}:18443")])
        .unwrap_or_default();
    let _ = ORIGINS.set(origins);

    let acceptor = match tls_acceptor() {
        Ok(acceptor) => acceptor,
        Err(error) => {
            eprintln!(
                "corral web: no usable certificate in {}: {error}",
                config_dir().join("tls").display()
            );
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("corral web: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve(acceptor)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("corral web: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Listeners from systemd (`LISTEN_FDS`), or the test ports when run by hand.
fn listeners() -> std::io::Result<Vec<std::net::TcpListener>> {
    let ours = std::env::var("LISTEN_PID").ok().and_then(|pid| pid.parse::<u32>().ok()) == Some(std::process::id());
    let count: i32 = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    if ours && count > 0 {
        // SAFETY: systemd passes these descriptors, starting at 3, for this process to own.
        return Ok((3..3 + count)
            .map(|fd| unsafe { std::net::TcpListener::from_raw_fd(fd) })
            .collect());
    }
    eprintln!("corral web: no sockets from systemd; testing on {TEST_HTTP} (http) and {TEST_HTTPS} (https)");
    Ok(vec![
        std::net::TcpListener::bind(TEST_HTTP)?,
        std::net::TcpListener::bind(TEST_HTTPS)?,
    ])
}

fn tls_acceptor() -> Result<TlsAcceptor, Box<dyn std::error::Error>> {
    let dir = config_dir().join("tls");
    let certs = CertificateDer::pem_file_iter(dir.join("fullchain.pem"))?.collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_file(dir.join("privkey.pem"))?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn serve(acceptor: TlsAcceptor) -> std::io::Result<()> {
    LAST_ACTIVE.store(now(), Ordering::SeqCst);
    for std_listener in listeners()? {
        std_listener.set_nonblocking(true)?;
        let port = std_listener.local_addr()?.port();
        let listener = TcpListener::from_std(std_listener)?;
        let https = port == 443 || port == 18443;
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let acceptor = acceptor.clone();
                LAST_ACTIVE.store(now(), Ordering::SeqCst);
                tokio::spawn(async move {
                    let http = hyper::server::conn::http1::Builder::new();
                    if https {
                        let Ok(tls) = acceptor.accept(stream).await else {
                            return;
                        };
                        let _ = http.serve_connection(TokioIo::new(tls), service_fn(forward)).await;
                    } else {
                        let _ = http.serve_connection(TokioIo::new(stream), service_fn(redirect)).await;
                    }
                });
            }
        });
    }

    // Exit when idle. Connections kept open for later are simply closed; the browser reconnects, and systemd starts
    // the web front again.
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let idle = now().saturating_sub(LAST_ACTIVE.load(Ordering::SeqCst));
        if BUSY.load(Ordering::SeqCst) == 0 && idle >= IDLE_EXIT.as_secs() {
            return Ok(());
        }
    }
}

/// Port 80: send the browser to the same address over HTTPS.
async fn redirect(req: Request<Incoming>) -> Result<Response<Body>, hyper::Error> {
    let _busy = Busy::start();
    let host = req
        .headers()
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .unwrap_or_default();
    let host = host.split(':').next().unwrap_or_default();
    let path = req.uri().path_and_query().map_or("/", |path| path.as_str());
    Ok(Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header(LOCATION, format!("https://{host}{path}"))
        .body(full(""))
        .expect("valid redirect"))
}

/// Port 443: forward to the gate, or say that Corral is off.
async fn forward(mut req: Request<Incoming>) -> Result<Response<Body>, hyper::Error> {
    let busy = Busy::start();
    // Corral's own endpoints and the worker replacement answer whether Corral is on or off (a browser checking its
    // worker while Corral is off still drops it)
    match (req.method().as_str(), req.uri().path()) {
        (_, "/sw.js") => return Ok(own(StatusCode::OK, "text/javascript", NO_WORKER)),
        ("GET", "/corral/status") => return Ok(own(StatusCode::OK, "application/json", panel::status())),
        ("POST", "/corral/arm") => {
            let header = |name: &str| req.headers().get(name).and_then(|value| value.to_str().ok());
            let origins = ORIGINS.get().map_or(&[][..], Vec::as_slice);
            if !panel::arm_allowed(origins, header("origin"), header(panel::ARM_HEADER)) {
                return Ok(own(
                    StatusCode::FORBIDDEN,
                    "text/plain",
                    "Only Corral's own page can open the gate.",
                ));
            }
            let result = tokio::task::spawn_blocking(panel::arm)
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            return Ok(match result {
                Ok(()) => own(StatusCode::ACCEPTED, "text/plain", "Corral armed."),
                Err(error) => own(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", error),
            });
        }
        _ => {}
    }
    let gate = *GATE.get().expect("set at start");
    let Ok(stream) = TcpStream::connect(gate).await else {
        // The icon is served as a success so browsers use it; everything else gets the off page
        return Ok(match req.uri().path() {
            "/favicon.svg" => own(StatusCode::OK, "image/svg+xml", FAVICON),
            _ => own(StatusCode::SERVICE_UNAVAILABLE, "text/html; charset=utf-8", OFF_PAGE),
        });
    };
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(connection);
    // The gate expects a path, not a full URL
    if let Some(path) = req.uri().path_and_query().cloned() {
        *req.uri_mut() = path.as_str().parse().expect("a path is a valid URI");
    }
    let response = sender.send_request(req).await?;

    // Only the page and the manifest carry the app's name; everything else streams through untouched
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let is_page = content_type.starts_with("text/html");
    let is_manifest = content_type.starts_with("application/manifest+json");
    if !response.status().is_success() || !(is_page || is_manifest) {
        // The body owns the Busy guard, so a long stream (a reply, the model list) counts until it ends
        return Ok(response.map(|body| {
            body.map_frame(move |frame| {
                let _busy = &busy;
                frame
            })
            .boxed()
        }));
    }

    let (mut parts, body) = response.into_parts();
    let bytes = body.collect().await?.to_bytes();
    let gzipped = parts.headers.get(CONTENT_ENCODING).is_some_and(|value| value == "gzip");
    let mut text = String::new();
    let readable = if gzipped {
        flate2::read::GzDecoder::new(&bytes[..])
            .read_to_string(&mut text)
            .is_ok()
    } else {
        std::str::from_utf8(&bytes).map(|s| text.push_str(s)).is_ok()
    };
    if !readable {
        // An unexpected encoding: pass the original through rather than break the page
        return Ok(Response::from_parts(parts, full(bytes)));
    }

    let renamed = if is_page {
        text.replacen("</head>", &format!("{HEAD_FIX}</head>"), 1)
    } else {
        text.replace("\"llama-ui\"", "\"Corral\"")
    };
    // Served plain now; drop the validators so the browser doesn't mix in an unrenamed cached copy
    parts.headers.remove(CONTENT_ENCODING);
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(ETAG);
    parts
        .headers
        .insert(CACHE_CONTROL, "no-cache".parse().expect("valid header"));
    Ok(Response::from_parts(parts, full(renamed)))
}
