use std::{pin::Pin, task::Poll};

use futures::{Stream, StreamExt};
use tokio::net::TcpStream;
use hyper_util::rt::TokioExecutor;
use tokio_rustls::server::TlsStream; // TODO: decide whether to use this or http2

use crate::accept::AcceptorStream;

use super::accept::AcceptError;

// Accepts new connections, negotiates tls upgrades for them, then serves them with a provided router
// #[pin_project]
pub struct ServerConnErrorStream
{
    // A stream of newly negotiated tls connections
    accept_stream: AcceptorStream,

    connector: RouterConnector
}

impl ServerConnErrorStream {
    pub fn new(
        accept_stream: AcceptorStream,
        service: axum::routing::Router<()>,
        handle: tokio::runtime::Handle,
        builder: &'static hyper_util::server::conn::auto::Builder<TokioExecutor>
    ) -> Self {
        Self {
            accept_stream,
            connector: RouterConnector::new(handle, builder, service)
        }
    }
}

impl Stream for ServerConnErrorStream
    {
    type Item = Result<
        (),
        AcceptError
    >;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        
        self.as_mut().accept_stream.poll_next_unpin(cx).map_ok(|conn| {
            self.connector.serve(conn)
        })
    }
}

// Takes newly formed tls connections, hands them off to serve_connection_with_upgrades(),
// then spawns UpgradeableConnection tasks with the handle
pub struct RouterConnector {
    // Needed both to spawn the UpgradeableConnection task, and to allow us to call
    // self.builder.serve_connection_with_upgrades() to create said UpgradeableConnection
    handle: tokio::runtime::Handle,

    // Used to build UpgradeableConnections. Needs to be static in order to keep tokio happy
    // Be sure to call enter() on self.handle before calling self.builder.serve_connection_with_upgrades()
    builder: &'static hyper_util::server::conn::auto::Builder<TokioExecutor>,

    // the axum router used to handle requests
    service: axum::routing::Router<()>,
}

impl RouterConnector {
    pub fn new(
        handle: tokio::runtime::Handle,
        builder: &'static hyper_util::server::conn::auto::Builder<TokioExecutor>,
        service: axum::routing::Router<()>,
    ) -> Self {
        Self {handle, builder, service}
    }

    pub fn serve(&self, io: TlsStream<TcpStream>) {
        let hyper_service = hyper_util::service::TowerToHyperService::new(
            self.service.clone()
        );

        // Needed in order for self.builder.serve_connection_with_upgrades() to run
        let guard = self.handle.enter();

        let served_conn = self.builder.serve_connection_with_upgrades(
            hyper_util::rt::tokio::TokioIo::new(io),
            hyper_service
        );

        tokio::spawn(
            served_conn
        );

        // TODO: figure out why tokio wants us to manually drop this.
        // I don't think it's all that important but it's surprising and I don't like not understanding why I do things
        drop(guard);
    }
}