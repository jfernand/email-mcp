//! Errors a front end may want to recognize, to tell the user what to do next.
//! Everything else is reported as a plain [`anyhow::Error`].

use std::fmt;

use crate::config::AuthMethod;

/// An operation could not run because setup is incomplete. Front ends can find it with
/// `err.downcast_ref::<SetupNeeded>()` and add instructions in their own terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupNeeded {
    /// No accounts are configured.
    NoAccounts,
    /// The account has no stored credential for its authentication method.
    NotLoggedIn { account: String, auth: AuthMethod },
    /// A Gmail OAuth account needs a Google OAuth client, and none is configured.
    NoGoogleOauthClient,
}

impl fmt::Display for SetupNeeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAccounts => write!(f, "no accounts configured"),
            Self::NotLoggedIn {
                account,
                auth: AuthMethod::Password,
            } => write!(
                f,
                "account '{account}' is not logged in (no password stored)"
            ),
            Self::NotLoggedIn {
                account,
                auth: AuthMethod::Oauth2,
            } => write!(
                f,
                "account '{account}' is not logged in (no Google sign-in stored)"
            ),
            Self::NoGoogleOauthClient => write!(f, "no Google OAuth client configured"),
        }
    }
}

impl std::error::Error for SetupNeeded {}
