use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use quinn::{ClientConfig, Endpoint, TransportConfig};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use solana_keypair::{Keypair, Signer};
use rustls::crypto::aws_lc_rs as provider;
use rustls::{DigitallySignedStruct, SignatureScheme};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};

const ALPN_TPU_PROTOCOL_ID: &[u8] = b"solana-tpu"; // Application protocol transported by TLS
pub const QUIC_KEEP_ALIVE: Duration = Duration::from_millis(1000);
pub const QUIC_MAX_TIMEOUT: Duration = Duration::from_millis(20000);

pub fn socket_addr_to_quic_server_name(peer: SocketAddr) -> String {
    format!("{}.{}.sol", peer.ip(), peer.port())
}

pub async fn new_quic_endpoint(keypair: &Keypair, client_port: u16) -> Endpoint {
    let root_store = rustls::RootCertStore::empty();

    let (cert, private_key) = new_x509_certificate(&keypair);

    let mut tls_config = rustls::ClientConfig::builder_with_provider(
        CryptoProvider {
            cipher_suites: vec![provider::cipher_suite::TLS13_AES_128_GCM_SHA256],
            kx_groups: vec![provider::kx_group::X25519],
            ..provider::default_provider()
        }
            .into(),
    )
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(root_store.clone())
        .with_client_auth_cert(vec![cert], private_key)
        .unwrap();

    tls_config.enable_early_data = true; // Allow 0-RTT from TLS 1.3
    tls_config.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];
    tls_config.enable_sni = false;

    let verifier = SkipServerVerification::new();
    tls_config.dangerous().set_certificate_verifier(verifier);

    // QUIC config
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls_config).unwrap()));
    let mut transport_config = TransportConfig::default();
    transport_config.max_idle_timeout(Some(
        QUIC_MAX_TIMEOUT.try_into().expect("Cannot convert timeout"),
    ));
    transport_config.keep_alive_interval(Some(
        QUIC_KEEP_ALIVE
            .try_into()
            .expect("Cannot convert keep alive"),
    ));
    transport_config.mtu_discovery_config(None);
    transport_config.min_mtu(1280);
    transport_config.send_fairness(false);
    config.transport_config(Arc::new(transport_config));

    // Local address
    let client_addr = SocketAddr::from(([0, 0, 0, 0], client_port));
    let mut endpoint = Endpoint::client(client_addr).expect("Cannot create endpoint");
    endpoint.set_default_client_config(config);
    endpoint
}

fn new_x509_certificate(keypair: &Keypair) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    const PKCS8_PREFIX: [u8; 16] = [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    let mut key_pkcs8_der = Vec::<u8>::with_capacity(PKCS8_PREFIX.len() + 32);
    key_pkcs8_der.extend_from_slice(&PKCS8_PREFIX);
    key_pkcs8_der.extend_from_slice(keypair.secret_bytes());

    let mut cert_der = Vec::<u8>::with_capacity(0xf4);
    cert_der.extend_from_slice(&[
        0x30, 0x81, 0xf6, 0x30, 0x81, 0xa9, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x08, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x30, 0x16,
        0x31, 0x14, 0x30, 0x12, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x0b, 0x53, 0x6f, 0x6c, 0x61,
        0x6e, 0x61, 0x20, 0x6e, 0x6f, 0x64, 0x65, 0x30, 0x20, 0x17, 0x0d, 0x37, 0x30, 0x30, 0x31,
        0x30, 0x31, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x5a, 0x18, 0x0f, 0x34, 0x30, 0x39, 0x36,
        0x30, 0x31, 0x30, 0x31, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x5a, 0x30, 0x00, 0x30, 0x2a,
        0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ]);
    cert_der.extend_from_slice(&keypair.pubkey().to_bytes());
    cert_der.extend_from_slice(&[
        0xa3, 0x29, 0x30, 0x27, 0x30, 0x17, 0x06, 0x03, 0x55, 0x1d, 0x11, 0x01, 0x01, 0xff, 0x04,
        0x0d, 0x30, 0x0b, 0x82, 0x09, 0x6c, 0x6f, 0x63, 0x61, 0x6c, 0x68, 0x6f, 0x73, 0x74, 0x30,
        0x0c, 0x06, 0x03, 0x55, 0x1d, 0x13, 0x01, 0x01, 0xff, 0x04, 0x02, 0x30, 0x00, 0x30, 0x05,
        0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x41, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    ]);

    (
        rustls::pki_types::CertificateDer::from(cert_der),
        rustls::pki_types::PrivateKeyDer::try_from(key_pkcs8_der).unwrap(),
    )
}

pub struct SkipServerVerification;

impl SkipServerVerification {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl Debug for SkipServerVerification {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "SkipServerVerification")
    }
}

impl ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}
