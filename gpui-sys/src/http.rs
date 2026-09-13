//! HTTPS transport for remote image assets (issue #103).
//!
//! gpui fetches `ImageSource::Resource(Resource::Uri(..))` through
//! [`App::http_client`], and that accessor answers with a `NullHttpClient`
//! until someone installs a real one. Zed ships that someone as the
//! `reqwest_client` crate; it is not published, so this module is the
//! equivalent: a [`HttpClient`] over the same `zed-reqwest` build
//! `gpui_http_client` already links, installed by [`install`] during window
//! startup.
//!
//! # Why a private Tokio runtime
//!
//! `reqwest`'s async client is a Tokio client: its timeouts are
//! `tokio::time::sleep` and its I/O driver is Tokio's. gpui polls futures on
//! its own smol-based executors, where polling those would find no reactor and
//! panic ("there is no reactor running"). Neither executor can drive the
//! other's timers, so the transport owns the boundary explicitly: one
//! dedicated thread runs a current-thread Tokio runtime forever, each request
//! is `spawn`ed onto it, and the caller awaits a `oneshot` on whatever executor
//! it already had.
//!
//! That boundary is also what keeps the guarantee the image path needs: the
//! socket read, the TLS handshake and the timeout all happen off the calling
//! thread, so a slow or unreachable host cannot stall a frame. The runtime is
//! built lazily on the first remote image, so a document with no remote
//! references never pays for the thread.

use futures::channel::oneshot;
use futures::future::BoxFuture;
use gpui::http_client::{AsyncBody, HttpClient, Result, Url, anyhow, http};

/// The lazily-built transport: a Tokio handle plus the reqwest client that
/// belongs to it.
struct Transport {
    handle: tokio::runtime::Handle,
    client: reqwest::Client,
}

/// Built at most once per process. `None` records a construction failure so a
/// broken runtime is reported to every request instead of being retried on
/// each one.
static TRANSPORT: std::sync::OnceLock<Option<Transport>> = std::sync::OnceLock::new();

/// Start the private runtime and build the client inside it.
///
/// The client must be built *inside* the runtime: reqwest captures the current
/// reactor when the client is created, so a client built outside would carry a
/// dead handle into every request. The thread therefore builds the runtime,
/// builds the client, publishes both, and then parks on a never-ready future —
/// which is what keeps the runtime's driver turning for every `spawn`ed
/// request.
fn transport() -> Option<&'static Transport> {
    TRANSPORT
        .get_or_init(|| {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("gpui-sys-http".to_string())
                .spawn(move || {
                    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        let _ = ready_tx.send(None);
                        return;
                    };
                    runtime.block_on(async move {
                        let client = reqwest::Client::builder()
                            .user_agent(USER_AGENT)
                            .build();
                        match client {
                            Ok(client) => {
                                let _ = ready_tx.send(Some(Transport {
                                    handle: tokio::runtime::Handle::current(),
                                    client,
                                }));
                            }
                            Err(_) => {
                                let _ = ready_tx.send(None);
                                return;
                            }
                        }
                        // Keep driving the reactor (and every spawned request)
                        // until the process exits.
                        std::future::pending::<()>().await
                    });
                })
                .ok()?;
            ready_rx.recv().ok().flatten()
        })
        .as_ref()
}

/// The `HttpClient` gpui's image loader downloads through.
///
/// Stateless: the runtime and client live in [`TRANSPORT`], so an instance is
/// just a handle to the process-wide transport.
pub struct ReqwestHttpClient;

impl HttpClient for ReqwestHttpClient {
    fn type_name(&self) -> &'static str {
        "ReqwestHttpClient"
    }

    fn user_agent(&self) -> Option<&http::HeaderValue> {
        // Set on the reqwest client instead, where it applies to every request
        // rather than only the ones gpui builds itself.
        None
    }

    fn send(&self, req: http::Request<AsyncBody>) -> BoxFuture<'static, Result<http::Response<AsyncBody>>> {
        let Some(transport) = transport() else {
            return Box::pin(async move { Err(anyhow!("gpui-sys: HTTP transport unavailable")) });
        };
        let handle = transport.handle.clone();
        let client = transport.client.clone();
        Box::pin(async move {
            let (done_tx, done_rx) = oneshot::channel();
            handle.spawn(async move {
                let _ = done_tx.send(perform(client, req).await);
            });
            match done_rx.await {
                Ok(result) => result,
                // The runtime is parked forever, so this only fires if the
                // process is tearing down mid-request.
                Err(_) => Err(anyhow!("gpui-sys: HTTP worker dropped the request")),
            }
        })
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }
}

const USER_AGENT: &str = concat!("gpui-sys/", env!("CARGO_PKG_VERSION"));

/// One request, translated between the two HTTP type stacks.
///
/// `http_client` and `reqwest` each re-export the `http` crate at the same
/// major version, so every construct crossing the boundary is converted
/// through its public surface (method string, URI string, header byte slices,
/// status code) rather than by assuming the two are literally the same type.
async fn perform(
    client: reqwest::Client,
    req: http::Request<AsyncBody>,
) -> Result<http::Response<AsyncBody>> {
    let (parts, body) = req.into_parts();
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?;
    let mut builder = client.request(method, parts.uri.to_string());
    for (name, value) in parts.headers.iter() {
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    let payload = read_body(body).await?;
    // An empty body is the absence of a body for our GETs; sending `Content-
    // Length: 0` on a GET is legal but needless noise at the server.
    if !payload.is_empty() {
        builder = builder.body(payload);
    }
    let response = builder.send().await?;
    let status = response.status();
    let headers = response.headers().clone();
    let payload = response.bytes().await?;
    let mut out = http::Response::builder().status(http::StatusCode::from_u16(status.as_u16())?);
    for (name, value) in headers.iter() {
        out = out.header(name.as_str(), value.as_bytes());
    }
    Ok(out.body(AsyncBody::from_bytes(payload))?)
}

/// Drain an `AsyncBody` into memory.
///
/// Image requests never stream a body, and consuming it before the request is
/// issued keeps the borrow of the caller's `AsyncBody` inside this future
/// instead of tying a `'static` boxed future to borrowed data.
async fn read_body(body: AsyncBody) -> Result<Vec<u8>> {
    use futures::io::AsyncReadExt;
    let mut body = body;
    let mut out = Vec::new();
    body.read_to_end(&mut out).await?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // End-to-end TLS round trip through the transport gpui's image loader uses.
    //
    // Ignored by default: it needs the network, and a red CI run for an offline
    // machine would say nothing about the code. Run it by hand with
    //
    //     cargo test --lib http::tests:: -- --ignored --nocapture
    //
    // It is worth keeping because nothing else in the suite covers the two
    // things most likely to break silently: the dedicated Tokio runtime
    // actually driving a request, and the rustls trust store resolving real
    // roots. Both failures are invisible until a user opens a document with an
    // `https://` image — which then just shows a placeholder forever.
    #[::core::prelude::v1::test]
    #[ignore = "requires network access"]
    fn fetches_a_png_over_https() {
        futures::executor::block_on(async {
            let client = ReqwestHttpClient;
            let response = client
                .get(
                    "https://raw.githubusercontent.com/wzzc-dev/MoUI/main/resource/branding/moonbud-mascot-100.png",
                    AsyncBody::empty(),
                    true,
                )
                .await
                .expect("HTTPS request should complete");
            assert_eq!(response.status().as_u16(), 200, "unexpected HTTP status");
            let body = read_body(response.into_body())
                .await
                .expect("response body should be readable");
            assert!(
                body.starts_with(b"\x89PNG"),
                "expected PNG magic, got {:?}",
                &body[..body.len().min(8)]
            );
        });
    }

    // Same runtime, no reachable network: a host that cannot resolve must come
    // back as an `Err` rather than hanging or panicking.
    #[::core::prelude::v1::test]
    #[ignore = "requires DNS to fail fast"]
    fn reports_an_unreachable_host_as_an_error() {
        futures::executor::block_on(async {
            let client = ReqwestHttpClient;
            let result = client
                .get(
                    "https://no-such-host.invalid/a.png",
                    AsyncBody::empty(),
                    true,
                )
                .await;
            assert!(result.is_err(), "an unresolvable host must not succeed");
        });
    }
}
