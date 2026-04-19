use std::{sync::Arc, collections::BTreeMap, string::FromUtf8Error};
use chrono::FixedOffset;
use data_encoding::SpecificationError;
use futures::{future::Either, StreamExt};
use k8s_openapi::{ByteString, api::core::v1::Secret};
use kube::runtime::{
    controller::Action, finalizer::{Error as FinalizerError, Event}, Controller
};
use tokio::sync::mpsc::error::SendError;
use tokio_rustls::{rustls::{
    pki_types::{
        CertificateDer, PrivateKeyDer, pem::PemObject
    }, ServerConfig
}, TlsAcceptor};

use crate::{accept::AcceptorWithExpiration, crd::{
    Certificate as CertManagerCert, CertificateStatus
}};


#[derive(Debug, thiserror::Error)]
pub enum CertControllerError {
    #[error("kube error in certificate controller: {0}")]
    Kube(#[from] kube::Error),
    // #[error("certificate controller terminated with Ok(()), which should never happen")]
    // Ok,
    #[error("Rustls server config error in certificate controller: {0}")]
    ServerConfig(#[from] ServerConfigError)
}

struct CertControllerContext {
    // Used for retrieving secrets produced by cert manager
    secrets_api: &'static kube::Api<Secret>,

    // Passed into kube::runtime::finalizer() in reconcile()
    certs_api: &'static kube::Api<CertManagerCert>,

    // Shared with an AcceptorStream
    sender: tokio::sync::mpsc::Sender<AcceptorWithExpiration>,

    // crypto_provider: Arc<CryptoProvider>
}


pub struct CertController();

impl CertController {

    // Reacts to certificate apply events by retrieving the associated secret data, constructing a ServerConfigWithExpiration, and uupdating the LatestAcceptor
    pub async fn run(
        namespace: String,
        kube_client: kube::Client,
        sender: tokio::sync::mpsc::Sender<AcceptorWithExpiration>,
        watcher_config: kube::runtime::watcher::Config
    ) -> Result<(), CertControllerError> {
        
        let secrets_api = kube::Api::<Secret>::namespaced(kube_client.clone(), namespace.as_str());
        let certs_api = kube::Api::<CertManagerCert>::namespaced(kube_client, namespace.as_str());
        let controller = Controller::<CertManagerCert>::new(
            certs_api.clone(),
            watcher_config
        );

        let context: Arc<CertControllerContext> = Arc::new(CertControllerContext {
            secrets_api: Box::leak(Box::new(secrets_api)),
            certs_api: Box::leak(Box::new(certs_api)),
            sender,
            // crypto_provider: Arc::new(
            //     tokio_rustls::rustls::crypto::aws_lc_rs::default_provider()
            // )
        });

        let controller = controller.reconcile_all_on(
            // Reconcile all managed objects when this controller is started
            // TODO: figure out if this happens anyway
            futures::stream::iter(std::iter::once(()))
        );

        Ok(controller.run(Self::reconcile, Self::error_policy, context)
        .for_each(|res| async move {
            // TODO: tracing
            match res {
                Err(e) => {
                    eprintln!("Certificate controller reconcile error: {:?}", e)
                },
                Ok(o) => println!("Certificate reconciled: {:?}", o)
            }
        }).await)
    }

    async fn reconcile(cert: Arc<CertManagerCert>, context: Arc<CertControllerContext>) -> Result<Action, kube::runtime::finalizer::Error<ServerConfigError>> {
        kube::runtime::finalizer(
            context.as_ref().certs_api,
            "challenge_server",
            cert,
            |event: Event<CertManagerCert>| {
                match event {
                    Event::Apply(c) => {
                        Either::Left(Self::apply_cert(c, context.as_ref()))
                    }, Event::Cleanup(c) => {
                        Either::Right(Self::cleanup_cert(c, context.as_ref()))
                    }
                }
            }
        ).await
    }

    async fn apply_cert(cert: Arc<CertManagerCert>, ctx: &CertControllerContext) -> Result<Action, ServerConfigError> {
        println!("applying cert");
        let cert_status = cert.status.as_ref().ok_or(ServerConfigError::NoStatus)?;
        // Get the certificate expiration so that it can be made into a CertificateWithExpiration
        let expiration = Self::try_parse_cert_expiration(cert_status)?;
        // Get the cert-manager secret associated with this certificate so we can make a rustls ServerConfig
        let secret = Self::get_secret(cert.as_ref(), ctx).await?;
        println!("fetched secret data");
        let secret_data_encoded = secret.data
        .ok_or(ServerConfigError::NoSecretData)?;
        // Build the rustls ServerConfig
        let server_config = Self::build_server_config(
            secret_data_encoded,
            // ctx.crypto_provider.clone()
        ).await?;
        println!("server config built");
        // Build a TlsAcceptor from the server config
        let tls_acceptor = TlsAcceptor::from(Arc::new(server_config));
        // Package it all together into an AcceptorWithExpiration that can be passed into the LatestAcceptor
        let acceptor_with_exp = AcceptorWithExpiration {
            acceptor: tls_acceptor,
            expiration: expiration
        };
        ctx.sender.send(acceptor_with_exp).await?;

        Ok(Action::await_change())
    }

    async fn cleanup_cert(_challenge: Arc<CertManagerCert>, _ctx: &CertControllerContext) -> Result<Action, ServerConfigError> {
        // Noop, since there's no harm in presenting an expired certificate so long as it's replaced by a valid one as soon as possible
        Ok(Action::await_change())
    }

    // TODO: tracing
    fn error_policy(_cert: Arc<CertManagerCert>, e: &FinalizerError<ServerConfigError>, _context: Arc<CertControllerContext>) -> Action {
        eprintln!("{:?}", e);
        if let FinalizerError::ApplyFailed(apply_err) = e {
            match apply_err  {
                _ => Action::requeue(tokio::time::Duration::from_secs(30))
            }
        } else  {
            Action::requeue(tokio::time::Duration::from_secs(30))
        }
    }

    // Try to get the secret produced by cert manager that corresponds to a given cert
    async fn get_secret(cert: &CertManagerCert, ctx: &CertControllerContext) -> Result<Secret, ServerConfigError> {
        ctx.secrets_api.get_opt(
            cert.spec.secret_name.as_str()
        ).await?.ok_or(ServerConfigError::DataNotFound)
    }

    fn try_parse_cert_expiration(status: &CertificateStatus) -> Result<chrono::DateTime<FixedOffset>, ServerConfigError> {
        let not_after_string = status.not_after.as_ref().ok_or(
            ServerConfigError::NoExpiration
        )?;
        Ok(
            chrono::DateTime::parse_from_str(
                not_after_string.as_str(), "%+"
            )?
        )
    }

    async fn build_server_config(
        mut secret_data: BTreeMap<String, ByteString>,
    ) -> Result<ServerConfig, ServerConfigError> {
        let key_data = secret_data.remove("tls.key").ok_or(ServerConfigError::DataNotFound)?;
        let cert_data = secret_data.remove("tls.crt").ok_or(ServerConfigError::DataNotFound)?;

        let cert_chain_result: Result<Vec<CertificateDer>, rustls::pki_types::pem::Error> = CertificateDer::pem_reader_iter(
            &mut std::io::Cursor::new(cert_data.0.into_boxed_slice())
        ).collect();
        let cert_chain = cert_chain_result?;

        // TODO: figure out why this isn't try_from since the bytes could be anything
        // let der = PrivatePkcs1KeyDer::<'static>::from(
        //     key_data.0
        // );

        // let key_string = String::from_utf8(
        //     key_data.0
        // )?;

        let der = PrivateKeyDer::from_pem_slice(
            &key_data.0
        )?;

        Ok(ServerConfig::builder()
        // .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            cert_chain, 
            der
        )?)
    }

}


#[derive(Debug, thiserror::Error)]
pub enum ServerConfigError {
    #[error("The cert manager secret had no data")]
    NoSecretData,
    #[error("The certificate status object Not After field was empty")]
    NoExpiration,
    #[error(transparent)]
    ExpirationParseError(#[from] chrono::ParseError),
    #[error(transparent)]
    Rustls(#[from] tokio_rustls::rustls::Error),
    #[error("The certificate object has not status")]
    NoStatus,
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error("No associated data found in secret object")]
    DataNotFound,
    #[error("INvalid specification for base64 decoder")]
    Spec(#[from] SpecificationError),
    #[error("Base64 data from secret wasn't valid utf-8")]
    FromUTF8(#[from] FromUtf8Error),
    #[error(transparent)]
    Send(#[from]SendError<AcceptorWithExpiration>),
    #[error("Error parsing cert chain: {0}")]
    Pem(#[from] rustls::pki_types::pem::Error),
}