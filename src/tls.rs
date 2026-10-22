//! One trust store for every TLS connection we make (IMAP, SMTP, OAuth).
//!
//! We use Mozilla's root list from `webpki-root-certs` (>= 1.0.10, which removed
//! the compromised "emSign ECC Root CA - G3") rather than the OS store, so a stale
//! system `ca-certificates` package can't reintroduce a distrusted root. As defense
//! in depth we also drop every remaining eMudhra/emSign root: reports of the October
//! 2026 key compromise disagree on which hierarchies are affected, and none of the
//! servers we talk to chain to them.

use std::sync::{Arc, LazyLock};

use tokio_rustls::rustls::{self, RootCertStore, pki_types::CertificateDer};

/// Byte strings that, when present in a root's DER, exclude it from our trust store.
const DISTRUSTED_ISSUERS: &[&[u8]] = &[b"eMudhra", b"emSign"];

fn is_distrusted(der: &[u8]) -> bool {
    DISTRUSTED_ISSUERS
        .iter()
        .any(|needle| {
            der.windows(needle.len())
                .any(|w| w == *needle)
        })
}

/// The trusted roots, as DER.
pub fn roots() -> impl Iterator<Item = &'static CertificateDer<'static>> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter(|c| !is_distrusted(c.as_ref()))
}

fn root_store() -> RootCertStore {
    let mut store = RootCertStore::empty();
    store.add_parsable_certificates(roots().cloned());
    store
}

pub fn client_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store())
                .with_no_client_auth(),
        )
    });
    CONFIG.clone()
}

/// Install the process-wide crypto provider. Several dependencies enable different
/// rustls backends, so rustls cannot pick one automatically.
pub fn init() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub fn http_client() -> anyhow::Result<reqwest::Client> {
    let certs = roots()
        .map(|c| reqwest::Certificate::from_der(c.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(reqwest::Client::builder()
        .tls_certs_only(certs)
        .build()?)
}

pub fn smtp_tls_parameters(
    domain: &str,
) -> anyhow::Result<lettre::transport::smtp::client::TlsParameters> {
    use lettre::transport::smtp::client::{Certificate, CertificateStore, TlsParameters};
    let mut builder =
        TlsParameters::builder(domain.to_owned()).certificate_store(CertificateStore::None);
    for c in roots() {
        builder = builder.add_root_certificate(Certificate::from_der(c.to_vec())?);
    }
    Ok(builder.build_rustls()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::rustls::pki_types::{TrustAnchor, pem::PemObject};

    /// The root whose intermediates' keys were compromised (October 2026).
    /// Copied from Ubuntu's ca-certificates 20260601 and checked byte-for-byte
    /// against the copy in webpki-root-certs 1.0.9. SHA-256 fingerprint:
    /// 86:A1:EC:BA:08:9C:4A:8D:3B:BE:27:34:C6:12:BA:34:1D:81:3E:04:3C:F9:E8:A8:62:CD:5C:57:A3:6B:BE:6B
    const EMSIGN_ECC_ROOT_CA_G3: &[u8] =
        include_bytes!("../tests/fixtures/emsign_ecc_root_ca_g3.pem");

    fn emsign_g3() -> (CertificateDer<'static>, TrustAnchor<'static>) {
        let cert = CertificateDer::from_pem_slice(EMSIGN_ECC_ROOT_CA_G3).unwrap();
        let mut store = RootCertStore::empty();
        store
            .add(cert.clone())
            .unwrap();
        (
            cert,
            store
                .roots
                .pop()
                .unwrap(),
        )
    }

    /// True if `store` would accept chains ending at `anchor`: same subject name,
    /// or the same key (catches a re-issued root carrying the compromised key).
    fn trusts(store: &RootCertStore, anchor: &TrustAnchor) -> bool {
        store
            .roots
            .iter()
            .any(|a| {
                a.subject == anchor.subject
                    || a.subject_public_key_info == anchor.subject_public_key_info
            })
    }

    #[test]
    fn emsign_ecc_root_ca_g3_is_not_trusted() {
        let (cert, anchor) = emsign_g3();
        assert!(
            String::from_utf8_lossy(&anchor.subject).contains("emSign ECC Root CA - G3"),
            "fixture is not the emSign ECC Root CA - G3 certificate"
        );

        // Not in the list handed to lettre (SMTP) and reqwest (OAuth)...
        assert!(roots().all(|c| c.as_ref() != cert.as_ref()));
        // ...nor in the store rustls verifies IMAP connections against.
        assert!(!trusts(&root_store(), &anchor));

        // Control: the check does detect the root when it is present.
        let mut with_g3 = root_store();
        with_g3
            .add(cert)
            .unwrap();
        assert!(trusts(&with_g3, &anchor));
    }

    #[test]
    fn no_emudhra_roots_trusted() {
        assert!(roots().all(|c| !is_distrusted(c.as_ref())));
        // The filter must actually be removing something, or the needles are wrong.
        assert!(roots().count() < webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
    }

    #[test]
    fn common_roots_still_trusted() {
        // Apple and Google mail chain to these; losing them would break everything.
        for name in [
            &b"ISRG Root X1"[..],
            b"GTS Root R1",
            b"DigiCert Global Root G2",
        ] {
            assert!(
                roots().any(|c| c
                    .as_ref()
                    .windows(name.len())
                    .any(|w| w == name)),
                "missing {}",
                String::from_utf8_lossy(name)
            );
        }
    }

    /// Real handshakes (no login) with every endpoint the presets use, through the
    /// same TLS setup the app uses. Needs network: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs network"]
    async fn provider_endpoints_verify_with_our_roots() {
        use lettre::{AsyncSmtpTransport, Tokio1Executor, transport::smtp::client::Tls};
        init();
        for host in ["imap.mail.me.com", "imap.gmail.com"] {
            let tcp = tokio::net::TcpStream::connect((host, 993))
                .await
                .unwrap();
            tokio_rustls::TlsConnector::from(client_config())
                .connect(rustls::pki_types::ServerName::try_from(host).unwrap(), tcp)
                .await
                .unwrap_or_else(|e| panic!("{host}: {e}"));
        }
        for (host, port, tls) in [
            (
                "smtp.mail.me.com",
                587,
                Tls::Required(smtp_tls_parameters("smtp.mail.me.com").unwrap()),
            ),
            (
                "smtp.gmail.com",
                465,
                Tls::Wrapper(smtp_tls_parameters("smtp.gmail.com").unwrap()),
            ),
        ] {
            let ok = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
                .port(port)
                .tls(tls)
                .build::<Tokio1Executor>()
                .test_connection()
                .await
                .unwrap_or_else(|e| panic!("{host}: {e}"));
            assert!(ok, "{host}");
        }
        http_client()
            .unwrap()
            .get("https://oauth2.googleapis.com/")
            .send()
            .await
            .expect("oauth2.googleapis.com");
    }
}
