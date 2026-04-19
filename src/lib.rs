use std::net::Ipv4Addr;

use accept::{AcceptError, AcceptorStream, AcceptorWithExpiration};
use controller::CertControllerError;
use futures::{FutureExt, StreamExt};
use hyper_util::rt::TokioExecutor;
use server::ServerConnErrorStream;

use lazy_static::lazy_static;

// *sigh*
// Tokio aparently doesn't like handling UpgradeableConnections that don't have a 'static lifetime
// Make sure to enter the runtime context before actually using this
lazy_static!(
    static ref BUILDER: hyper_util::server::conn::auto::Builder<TokioExecutor> = {
        hyper_util::server::conn::auto::Builder::new(
            TokioExecutor::new()
        )
    };
);

pub mod accept;
pub mod server;
mod crd;
mod controller;

// TODO: implement https://docs.rs/axum/latest/axum/serve/trait.Listener.html on... something?

// TODO: graceful shutdown
pub async fn serve_cert_manager_https(
    watcher_config: kube::runtime::watcher::Config,
    namespace: String,
    kube_client: kube::Client,
    port: Option<u16>,
    addr: Option<std::net::IpAddr>,
    router: axum::Router<()>
) -> Result<(), std::io::Error> {
    // TODO: Require a CryptoProvider
    // Use IPV4 0.0.0.0 and port 443 as defaults
    let socket = std::net::SocketAddr::new(
        addr.unwrap_or(
            std::net::IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))
        ),
        port.unwrap_or(443)
    );
    let std_listener = std::net::TcpListener::bind(socket)?;
        
    // tokio::net::TcpListener::from_std requires std_listener to be in nonblocking mode
    std_listener.set_nonblocking(true)?;
    let tcp_listener = tokio::net::TcpListener::from_std(std_listener)?;

    // Shared between the AcceptorStream and the controller
    let (
        sender, receiver
        // TODO: figure out what a reasonable buffer size is
    ) = tokio::sync::mpsc::channel::<AcceptorWithExpiration>(10);

    let acceptor_stream = AcceptorStream::new(
        tcp_listener,
        tokio::runtime::Handle::current(),
        receiver
    );

    let cert_controller_fut = controller::CertController::run(
        namespace.clone(),
        kube_client,
        sender,
        watcher_config
    ).then(|res| async {
        match res {
            Err(e) => {
                eprintln!("{}", e);
            },
            Ok(()) => ()
        }
    });

    // Again, make sure we enter a runtime context before using this
    let builder: &'static hyper_util::server::conn::auto::Builder<TokioExecutor> = &BUILDER;

    let server_fut = ServerConnErrorStream::new(
        acceptor_stream,
        router,
        tokio::runtime::Handle::current(),
        builder
    ).for_each(|item| async move {
        match item {
            Ok(()) => (),
            Err(e) => {
                eprintln!("{}", e);
            }
        }
    });

    let controller_handle = tokio::spawn(cert_controller_fut);
    let server_handle = tokio::spawn(server_fut);

    futures::future::join(
        controller_handle, server_handle
    ).then(|(controller_result, server_result)| async {
        match controller_result {
            Err(e) => {
                eprintln!("Controller join errer: {}", e);
            },
            Ok(()) => {
                match server_result {
                    Ok(()) => (),
                    Err(e) => {
                        eprintln!("Server join errer: {}", e)
                    }
                }
            }
        }
    }).await;

    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum ControllerOrServerError {
    #[error("Cert controller errror: {0}")]
    Controller(CertControllerError),
    #[error("Error starting new server connection: {0}")]
    Server(AcceptError)
}