use std::{pin::Pin, task::Poll};
use chrono::FixedOffset;
use futures::{stream::FuturesUnordered, Stream, StreamExt, TryFutureExt};
use tokio::{net::{TcpListener, TcpStream}, task::{JoinError, JoinHandle}};
use tokio_rustls::server::TlsStream;
use tokio_stream::wrappers::TcpListenerStream;

#[derive(Clone)]
pub struct AcceptorWithExpiration {
    pub expiration: chrono::DateTime<FixedOffset>,

    pub acceptor: tokio_rustls::TlsAcceptor,
}

#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    #[error("Error during tcp connection establishment: {0}")]
    TcpConnectError(std::io::Error),
    #[error("Error during tls handshake: {0}")]
    TlsHandshakeError(std::io::Error),
    #[error("No acceptor present, probably because the certificate and/or its associated secret are not ready")]
    NoAcceptor,
    #[error("Join error from tls upgrade future: {0}")]
    JoinError(JoinError),
    #[error("Calling poll_next on tcp listener stream returned a None")]
    TcpListenerStreamNone
}

// Abstracts over the process of upgrading connections from a TcpListenerStream using the
// acceptor in a LatestAcceptor. On its own, this doesn't do anything to keep the LatestAcceptor
// up to date. It is expected that the certificate controller is keeping it up to date.
// TODO: tracing
pub struct AcceptorStream {
    listener_stream: TcpListenerStream,
    // Used for spawning new connection tasks
    handle: tokio::runtime::Handle,
    
    tls_upgrades: Pin<Box<FuturesUnordered<JoinHandle<TlsResult>>>>,

    latest_acceptor: Option<AcceptorWithExpiration>,

    acceptor_receiver: tokio::sync::mpsc::Receiver<AcceptorWithExpiration>,
}

impl AcceptorStream {
    pub fn new(
        tcp_listener: TcpListener,
        handle: tokio::runtime::Handle,
        acceptor_receiver: tokio::sync::mpsc::Receiver<AcceptorWithExpiration>
    ) -> Self {
        Self {
            listener_stream: TcpListenerStream::new(tcp_listener),
            handle,
            tls_upgrades: Box::pin(FuturesUnordered::new()),
            latest_acceptor: None,
            acceptor_receiver
        }
    }

    // If an acceptor is ready, then for each new tcp connection, start the
    // tls handshake process and add the newly created tls handshake future to self.handshakes
    // Note: futures added to self.handshakes this way will not start sending wake-ups
    // to the context until poll_next is called on self.handshakes. In practice,
    // self.handshakes is polled immediately after calls to this method in Self::poll_next()
    fn poll_tls_upgrades(self: &mut Self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), AcceptError>> {
        match self.poll_latest_acceptor(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(acceptor_with_exp) => {
                self.start_tls_upgrades(cx, &acceptor_with_exp)
            }
        }
    }

    // For each new tcp connection, start a tls upgrade using the provided acceptor
    fn start_tls_upgrades(
        self: &mut Self,
        cx: &mut std::task::Context<'_>,
        acceptor_with_exp: &AcceptorWithExpiration
    ) -> Poll<Result<(), AcceptError>> {
        while let Poll::Ready(Some(
            tcp_stream_result
        )) = self.listener_stream.poll_next_unpin(cx) {
            match tcp_stream_result {
                // If an error is found, return early with it
                Err(e) => return Poll::Ready(Err(AcceptError::TcpConnectError(e))),
                Ok(tcp_stream) => {
                    // Spawn the upgrade as a new task, keeping a handle to it
                    let handshake_join_handle = self.handle.spawn(
                        acceptor_with_exp.acceptor.accept(
                            tcp_stream
                        ).map_err(|e| {
                            AcceptError::TlsHandshakeError(e)
                        })
                    );
                    // Push the handle to the local collection
                    self.tls_upgrades.push(handshake_join_handle);
                }
            }
        }
        Poll::Ready(Ok(()))
    }

    // Get the latest acceptor now or schedule a wakeup when one is ready
    fn poll_latest_acceptor(self: &mut Self, cx: &mut std::task::Context<'_>) -> Poll<AcceptorWithExpiration> {
        // Poll the receiver for new acceptors
        // TODO: decide whether we instead want to do this one at a time
        while let Poll::Ready(
            Some(acceptor)
        ) = self.acceptor_receiver.poll_recv(cx) {
            // If the acceptor we get for the receiver has a later expiration, replace
            // the store acceptor with it
            if self.should_replace_acceptor(&acceptor) {
                self.latest_acceptor = Some(acceptor)
            }
        }

        // If no acceptors have been provided yet, this task will be woken up when one has
        // been sent by the receiver
        match self.latest_acceptor.as_ref() {
            Some(acceptor) => {
                Poll::Ready(acceptor.clone())
            },
            None => {
                println!("Acceptor not available");
                Poll::Pending
            }
        }
    }

    fn should_replace_acceptor(&self, incoming_acceptor: &AcceptorWithExpiration) -> bool {
        match self.latest_acceptor.as_ref() {
            None => true,
            Some(existing_acceptor) => {
                // If the incoming value has a greater(later) expiration than the existing value,
                // replace the old value with the new value
                incoming_acceptor.expiration > existing_acceptor.expiration
            }
        }
    }
}

type TlsResult = Result<TlsStream<TcpStream>, AcceptError>;

impl Stream for AcceptorStream {
    type Item = Result<TlsStream<TcpStream>, AcceptError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<Self::Item>> {
        // poll for new tcp connections to be upgraded, returning any errors found
        match self.as_mut().poll_tls_upgrades(cx) {
            Poll::Ready(Err(e)) => {
                return Poll::Ready(Some(Err(e)))
            },
            Poll::Ready(Ok(())) | Poll::Pending => ()
        }
        
        // poll for finished TLS upgrades
        match self.tls_upgrades.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,

            // "None" means that we've reached the end of the FuturesUnordered, but we
            // shouldn't return Ready(None) because that would signal the end of the stream
            Poll::Ready(None) => Poll::Pending,
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(AcceptError::JoinError(e)))),
            Poll::Ready(Some(Ok(Err(e)))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(Some(Ok(Ok(tls_stream)))) => Poll::Ready(Some(Ok(tls_stream)))
        }
    }
}
