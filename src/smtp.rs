use std::time::Duration;

use anyhow::{Context, Result};
use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::Tls;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::config::{Account, SmtpSecurity};
use crate::imap::Credential;

fn transport(account: &Account, cred: &Credential) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let params = crate::tls::smtp_tls_parameters(&account.smtp_host)?;
    let tls = match account.smtp_security {
        SmtpSecurity::Tls => Tls::Wrapper(params),
        SmtpSecurity::Starttls => Tls::Required(params),
    };
    let (secret, mechanisms) = match cred {
        Credential::Password(p) => (p.clone(), vec![Mechanism::Plain, Mechanism::Login]),
        Credential::OauthToken(t) => (t.clone(), vec![Mechanism::Xoauth2]),
    };
    Ok(
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&account.smtp_host)
            .port(account.smtp_port)
            .tls(tls)
            .credentials(Credentials::new(account.username().to_owned(), secret))
            .authentication(mechanisms)
            .timeout(Some(Duration::from_secs(60)))
            .build(),
    )
}

pub async fn send(account: &Account, cred: &Credential, envelope: &Envelope, raw: &[u8]) -> Result<()> {
    transport(account, cred)?
        .send_raw(envelope, raw)
        .await
        .with_context(|| format!("sending via {}", account.smtp_host))?;
    Ok(())
}

pub async fn test(account: &Account, cred: &Credential) -> Result<()> {
    transport(account, cred)?
        .test_connection()
        .await
        .with_context(|| format!("connecting to {}", account.smtp_host))?;
    Ok(())
}
