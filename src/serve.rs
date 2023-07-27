use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::{self, Body};
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, get_service, Router};
use axum::Server;
use axum_server::Handle;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use crate::common::{LOCAL, NETWORK, SERVER};
use crate::config::RtcServe;
use crate::proxy::{ProxyHandlerHttp, ProxyHandlerWebSocket};
use crate::watch::WatchSystem;

const INDEX_HTML: &str = "index.html";

/// A system encapsulating a build & watch system, responsible for serving generated content.
pub struct ServeSystem {
    cfg: Arc<RtcServe>,
    watch: WatchSystem,
    http_addr: String,
    shutdown_tx: broadcast::Sender<()>,
    //  N.B. we use a broadcast channel here because a watch channel triggers a
    //  false positive on the first read of channel
    build_done_chan: broadcast::Sender<()>,
}

impl ServeSystem {
    /// Construct a new instance.
    pub async fn new(cfg: Arc<RtcServe>, shutdown: broadcast::Sender<()>) -> Result<Self> {
        let (build_done_chan, _) = broadcast::channel(8);
        let watch = WatchSystem::new(
            cfg.watch.clone(),
            shutdown.clone(),
            Some(build_done_chan.clone()),
        )
        .await?;
        let prefix = if cfg.tls.is_some() { "https" } else { "http" };
        let http_addr = format!(
            "{}://{}:{}{}",
            prefix, cfg.address, cfg.port, &cfg.watch.build.public_url
        );
        Ok(Self {
            cfg,
            watch,
            http_addr,
            shutdown_tx: shutdown,
            build_done_chan,
        })
    }

    /// Run the serve system.
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn run(mut self) -> Result<()> {
        // Spawn the watcher & the server.
        let _build_res = self.watch.build().await; // TODO: only open after a successful build.
        let watch_handle = tokio::spawn(self.watch.run());
        let server_handle = Self::spawn_server(
            self.cfg.clone(),
            self.shutdown_tx.subscribe(),
            self.build_done_chan,
        )
        .await?;

        // Open the browser.
        if self.cfg.open {
            if let Err(err) = open::that(self.http_addr) {
                tracing::error!(error = ?err, "error opening browser");
            }
        }
        drop(self.shutdown_tx); // Drop the broadcast channel to ensure it does not keep the system alive.
        if let Err(err) = watch_handle.await {
            tracing::error!(error = ?err, "error joining watch system handle");
        }
        if let Err(err) = server_handle.await {
            tracing::error!(error = ?err, "error joining server handle");
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(cfg, shutdown_rx))]
    async fn spawn_server(
        cfg: Arc<RtcServe>,
        mut shutdown_rx: broadcast::Receiver<()>,
        build_done_chan: broadcast::Sender<()>,
    ) -> Result<JoinHandle<()>> {
        // Build a shutdown signal for the warp server.
        let graceful_shutdown_handle = Handle::new();
        let handle_clone = graceful_shutdown_handle.clone();
        let shutdown_fut = async move {
            // Any event on this channel, even a drop, should trigger shutdown.
            let _res = shutdown_rx.recv().await;
            tracing::debug!("server is shutting down");
            handle_clone.graceful_shutdown(Some(Duration::from_secs(0)));
        };

        // Build the proxy client.
        let client = reqwest::ClientBuilder::new()
            .http1_only()
            .build()
            .context("error building proxy client")?;

        let insecure_client = reqwest::ClientBuilder::new()
            .http1_only()
            .danger_accept_invalid_certs(true)
            .build()
            .context("error building insecure proxy client")?;

        // Build the server.
        let state = Arc::new(State::new(
            cfg.watch.build.final_dist.clone(),
            cfg.watch.build.public_url.clone(),
            client,
            insecure_client,
            &cfg,
            build_done_chan,
        ));
        let router = router(state, cfg.clone());
        let addr = (cfg.address, cfg.port).into();

        let mut http_server: Option<_> = None;
        let mut https_server: Option<_> = None;
        if let Some(tls_config) = cfg.tls.clone() {
            // Spawn a task to gracefully shutdown server.
            tokio::spawn(shutdown_fut);
            https_server = Some(
                axum_server::bind_rustls(addr, tls_config)
                    .handle(graceful_shutdown_handle)
                    .serve(router.into_make_service()),
            );
        } else {
            http_server = Some(
                Server::bind(&addr)
                    .serve(router.into_make_service())
                    .with_graceful_shutdown(shutdown_fut),
            );
        }

        let prefix = if cfg.tls.is_some() { "https" } else { "http" };
        if addr.ip().is_unspecified() {
            let addresses = local_ip_address::list_afinet_netifas()
                .map(|addrs| {
                    addrs
                        .into_iter()
                        .filter_map(|(_, ipaddr)| match ipaddr {
                            IpAddr::V4(ip) if ip.is_private() || ip.is_loopback() => Some(ip),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|_| vec![Ipv4Addr::LOCALHOST]);
            tracing::info!(
                "{} server listening at:\n{}",
                SERVER,
                addresses
                    .iter()
                    .map(|address| format!(
                        "    {} {}://{}:{}",
                        if address.is_loopback() {
                            LOCAL
                        } else {
                            NETWORK
                        },
                        prefix,
                        address,
                        cfg.port
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        } else {
            tracing::info!("{} server listening at {}://{}", SERVER, prefix, addr);
        }
        // Block this routine on the server's completion.
        Ok(tokio::spawn(async move {
            if let Some(server) = http_server {
                if let Err(err) = server.await {
                    tracing::error!(error = ?err, "error from server task");
                }
            }
            if let Some(server) = https_server {
                if let Err(err) = server.await {
                    tracing::error!(error = ?err, "error from server task");
                }
            }
        }))
    }
}

/// Server state.
pub struct State {
    /// A client instance used by proxies.
    pub client: reqwest::Client,
    /// A client instance used by proxies to make insecure requests.
    pub insecure_client: reqwest::Client,
    /// The location of the dist dir.
    pub dist_dir: PathBuf,
    /// The public URL from which assets are being served.
    pub public_url: String,
    /// The channel to receive build_done notifications on.
    pub build_done_chan: broadcast::Sender<()>,
    /// Whether to disable autoreload
    pub no_autoreload: bool,
}

impl State {
    /// Construct a new instance.
    pub fn new(
        dist_dir: PathBuf,
        public_url: String,
        client: reqwest::Client,
        insecure_client: reqwest::Client,
        cfg: &RtcServe,
        build_done_chan: broadcast::Sender<()>,
    ) -> Self {
        Self {
            client,
            insecure_client,
            dist_dir,
            public_url,
            build_done_chan,
            no_autoreload: cfg.no_autoreload,
        }
    }
}

/// Build the Trunk router, this includes that static file server, the WebSocket server,
/// (for autoreload & HMR in the future), as well as any user-defined proxies.
fn router(state: Arc<State>, cfg: Arc<RtcServe>) -> Router {
    // Build static file server, middleware, error handler & WS route for reloads.
    let public_route = if state.public_url == "/" {
        &state.public_url
    } else {
        state
            .public_url
            .strip_suffix('/')
            .unwrap_or(&state.public_url)
    };

    let mut router = Router::new()
        .fallback_service(
            Router::new().nest_service(
                public_route,
                get_service(
                    ServeDir::new(&state.dist_dir)
                        .fallback(ServeFile::new(state.dist_dir.join(INDEX_HTML))),
                )
                .handle_error(|error| async move {
                    tracing::error!(?error, "failed serving static file");
                    StatusCode::INTERNAL_SERVER_ERROR
                })
                .layer(TraceLayer::new_for_http()),
            ),
        )
        .route(
            "/_trunk/ws",
            get(
                |ws: WebSocketUpgrade, state: axum::extract::State<Arc<State>>| async move {
                    ws.on_upgrade(|socket| async move { handle_ws(socket, state.0).await })
                },
            ),
        )
        .with_state(state.clone());

    tracing::info!(
        "{} serving static assets at -> {}",
        SERVER,
        state.public_url.as_str()
    );

    // Build proxies.
    if let Some(backend) = &cfg.proxy_backend {
        if cfg.proxy_ws {
            let handler = ProxyHandlerWebSocket::new(backend.clone(), cfg.proxy_rewrite.clone());
            router = handler.clone().register(router);
            tracing::info!(
                "{} proxying websocket {} -> {}",
                SERVER,
                handler.path(),
                &backend
            );
        } else {
            let client = if cfg.proxy_insecure {
                state.insecure_client.clone()
            } else {
                state.client.clone()
            };

            let handler = ProxyHandlerHttp::new(client, backend.clone(), cfg.proxy_rewrite.clone());
            router = handler.clone().register(router);
            tracing::info!("{} proxying {} -> {}", SERVER, handler.path(), &backend);
        }
    } else if let Some(proxies) = &cfg.proxies {
        for proxy in proxies.iter() {
            if proxy.ws {
                let handler =
                    ProxyHandlerWebSocket::new(proxy.backend.clone(), proxy.rewrite.clone());
                router = handler.clone().register(router);
                tracing::info!(
                    "{} proxying websocket {} -> {}",
                    SERVER,
                    handler.path(),
                    &proxy.backend
                );
            } else {
                let client = if proxy.insecure {
                    state.insecure_client.clone()
                } else {
                    state.client.clone()
                };

                let handler =
                    ProxyHandlerHttp::new(client, proxy.backend.clone(), proxy.rewrite.clone());
                router = handler.clone().register(router);
                tracing::info!(
                    "{} proxying {} -> {}",
                    SERVER,
                    handler.path(),
                    &proxy.backend
                );
            };
        }
    }

    router
}

async fn handle_ws(mut ws: WebSocket, state: Arc<State>) {
    let mut rx = state.build_done_chan.subscribe();
    tracing::debug!("autoreload websocket opened");
    while tokio::select! {
        _ = ws.recv() => {
            tracing::debug!("autoreload websocket closed");
            return
        }
        build_done = rx.recv() => build_done.is_ok(),
    } {
        let ws_send = ws.send(axum::extract::ws::Message::Text(
            r#"{"reload": true}"#.to_owned(),
        ));
        if ws_send.await.is_err() {
            break;
        }
    }
}

/// A result type used to work seamlessly with axum.
pub(crate) type ServerResult<T> = std::result::Result<T, ServerError>;

/// A newtype to make anyhow errors work with axum.
pub(crate) struct ServerError(pub anyhow::Error);

impl From<anyhow::Error> for ServerError {
    fn from(src: anyhow::Error) -> Self {
        ServerError(src)
    }
}

impl axum::response::IntoResponse for ServerError {
    fn into_response(self) -> Response {
        tracing::error!(error = ?self.0, "error handling request");
        let mut res = Response::new(body::boxed(Body::empty()));
        *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        res
    }
}
