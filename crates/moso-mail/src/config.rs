//! Backend choice, the default sender, and the staging safety valve — as
//! configuration rather than as code.
//!
//! Which mailer a process uses is a deployment decision, not a code decision.
//! An application builds one [`MailConfig`] from its typed config and hands it
//! to [`MailConfig::build`]; nothing in the request path names a backend.

use std::time::Duration;

use moso_core::config::SecretString;

use crate::{Address, Mailer, Result};

/// Which backend a process sends through.
///
/// ```
/// use moso_mail::MailBackendKind;
///
/// assert_eq!(MailBackendKind::default(), MailBackendKind::Console);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MailBackendKind {
    /// Print, and serve the preview inbox. The development default.
    #[default]
    Console,
    /// Write `.eml` files.
    File,
    /// Keep everything in memory. Tests.
    Memory,
    /// Pooled SMTP.
    Smtp,
    /// Amazon SES.
    Ses,
    /// SendGrid.
    Sendgrid,
    /// Postmark.
    Postmark,
    /// Resend.
    Resend,
    /// Mailgun.
    Mailgun,
}

impl MailBackendKind {
    /// Parse the value of a `MAIL_BACKEND` variable.
    ///
    /// ```
    /// use moso_mail::MailBackendKind;
    ///
    /// assert_eq!(MailBackendKind::parse("smtp"), Some(MailBackendKind::Smtp));
    /// assert_eq!(MailBackendKind::parse("  SMTP "), Some(MailBackendKind::Smtp));
    /// assert_eq!(MailBackendKind::parse("carrier-pigeon"), None);
    /// ```
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "console" => Some(Self::Console),
            "file" => Some(Self::File),
            "memory" => Some(Self::Memory),
            "smtp" => Some(Self::Smtp),
            "ses" => Some(Self::Ses),
            "sendgrid" => Some(Self::Sendgrid),
            "postmark" => Some(Self::Postmark),
            "resend" => Some(Self::Resend),
            "mailgun" => Some(Self::Mailgun),
            _ => None,
        }
    }

    /// Every name [`parse`](MailBackendKind::parse) accepts, in order.
    ///
    /// What an unknown-backend error suggests from, and what `.env.example`
    /// lists next to `MAIL_BACKEND`.
    ///
    /// ```
    /// use moso_mail::MailBackendKind;
    ///
    /// assert!(MailBackendKind::NAMES.contains(&"resend"));
    /// ```
    pub const NAMES: &'static [&'static str] = &[
        "console", "file", "memory", "smtp", "ses", "sendgrid", "postmark", "resend", "mailgun",
    ];

    /// The cargo feature that has to be on for this backend to build.
    ///
    /// Named in the error, because "unknown backend `ses`" when the real
    /// problem is a feature flag is the least helpful message in the crate.
    ///
    /// ```
    /// use moso_mail::MailBackendKind;
    ///
    /// assert_eq!(MailBackendKind::Ses.feature(), "mail-ses");
    /// ```
    #[must_use]
    pub const fn feature(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::File => "file",
            Self::Memory => "memory",
            Self::Smtp => "mail-smtp",
            Self::Ses => "mail-ses",
            Self::Sendgrid => "mail-sendgrid",
            Self::Postmark => "mail-postmark",
            Self::Resend => "mail-resend",
            Self::Mailgun => "mail-mailgun",
        }
    }

    /// Whether this backend needs [`MailConfig::url`] set.
    ///
    /// ```
    /// use moso_mail::MailBackendKind;
    ///
    /// assert!(MailBackendKind::Smtp.needs_url());
    /// assert!(!MailBackendKind::Console.needs_url());
    /// ```
    #[must_use]
    pub const fn needs_url(self) -> bool {
        !self.is_local()
    }

    /// The name this parses from.
    ///
    /// ```
    /// use moso_mail::MailBackendKind;
    ///
    /// assert_eq!(MailBackendKind::Console.as_str(), "console");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::File => "file",
            Self::Memory => "memory",
            Self::Smtp => "smtp",
            Self::Ses => "ses",
            Self::Sendgrid => "sendgrid",
            Self::Postmark => "postmark",
            Self::Resend => "resend",
            Self::Mailgun => "mailgun",
        }
    }

    /// Whether this backend needs no external service.
    ///
    /// The boot log warns when a production profile picks one of these, because
    /// it means the deployment silently sends nothing.
    ///
    /// ```
    /// use moso_mail::MailBackendKind;
    ///
    /// assert!(MailBackendKind::Console.is_local());
    /// assert!(!MailBackendKind::Smtp.is_local());
    /// ```
    #[must_use]
    pub const fn is_local(self) -> bool {
        matches!(self, Self::Console | Self::File | Self::Memory)
    }
}

/// Everything a process needs to send mail.
///
/// ```no_run
/// use moso_mail::{MailBackendKind, MailConfig};
///
/// let config = MailConfig::new("Shop <hello@shop.example>", MailBackendKind::Smtp)?
///     .url("smtp://user:pass@mail.example.com:587")
///     .timeout(std::time::Duration::from_secs(10));
/// config.validate()?;
/// # Ok::<(), moso_mail::Error>(())
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MailConfig {
    /// The `From` used by any message that does not set its own.
    pub from: Address,
    /// Which backend to build.
    pub backend: MailBackendKind,
    /// The DSN or API key, depending on the backend. Secret either way.
    pub url: Option<SecretString>,
    /// The region, for providers that have them.
    pub region: Option<String>,
    /// The sending domain, for providers that need it.
    pub domain: Option<String>,
    /// The directory the `file` backend writes to.
    pub directory: Option<std::path::PathBuf>,
    /// How long a single send may take before it is abandoned.
    ///
    /// An enforced deadline, not advice: [`build`](MailConfig::build) hands it
    /// to whichever backend it constructs, and that backend wraps the whole of
    /// its send in it. Overrunning is
    /// [`Error::Timeout`](crate::Error::Timeout) — a 504, retryable, naming
    /// the backend and this duration. See [`crate::deadline`].
    pub timeout: Duration,
    /// Send everything to this address instead of the real recipients.
    ///
    /// The staging safety valve. Set it and it is *impossible* to mail a real
    /// customer from that environment, whatever a handler does.
    pub redirect_to: Option<Address>,
    /// Whether to serve the preview inbox at `/_mail`.
    ///
    /// Defaults to on for a local backend and off otherwise; a production
    /// profile that turns it on gets a boot warning, because the inbox shows
    /// message bodies.
    pub preview: bool,
    /// Whether to consult the suppression list. Off only for a test that is
    /// exercising the suppression list itself.
    pub suppression: bool,
}

impl MailConfig {
    /// A configuration with the documented defaults.
    ///
    /// # Errors
    ///
    /// [`Error::Address`](crate::Error::Address) when `from` is not a mailbox.
    ///
    /// ```
    /// use moso_mail::{MailBackendKind, MailConfig};
    ///
    /// let config = MailConfig::new("hello@example.com", MailBackendKind::Console)?;
    /// assert_eq!(config.timeout, std::time::Duration::from_secs(30));
    /// assert!(config.preview, "a local backend serves the inbox by default");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    pub fn new(from: &str, backend: MailBackendKind) -> Result<Self> {
        Ok(Self {
            from: parse_from(from)?,
            backend,
            url: None,
            region: None,
            domain: None,
            directory: None,
            timeout: crate::deadline::DEFAULT_TIMEOUT,
            redirect_to: None,
            // On for a local backend and off otherwise: the inbox shows message
            // bodies, and message bodies contain password-reset links.
            preview: backend.is_local(),
            suppression: true,
        })
    }

    /// Set the DSN or API key.
    ///
    /// ```
    /// # use moso_mail::{MailBackendKind, MailConfig};
    /// let config = MailConfig::new("a@b.com", MailBackendKind::Smtp)?
    ///     .url("smtp://localhost:1025?security=none");
    /// assert!(config.url.is_some());
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(SecretString::new(url.into()));
        self
    }

    /// Set the per-send deadline. Default 30 seconds.
    ///
    /// Every backend this configuration builds enforces it.
    ///
    /// ```
    /// # use moso_mail::{MailBackendKind, MailConfig};
    /// let config = MailConfig::new("a@b.com", MailBackendKind::Console)?
    ///     .timeout(std::time::Duration::from_secs(5));
    /// assert_eq!(config.timeout.as_secs(), 5);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Redirect every message to one address.
    ///
    /// ```
    /// # use moso_mail::{Address, MailBackendKind, MailConfig};
    /// let config = MailConfig::new("a@b.com", MailBackendKind::Console)?
    ///     .redirect_to(Address::new("staging@shop.example")?);
    /// assert!(config.redirect_to.is_some());
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn redirect_to(mut self, address: Address) -> Self {
        self.redirect_to = Some(address);
        self
    }

    /// Set the directory the `file` backend writes to.
    ///
    /// ```
    /// # use moso_mail::{MailBackendKind, MailConfig};
    /// let _ = MailConfig::new("a@b.com", MailBackendKind::File)?.directory("target/mail");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn directory(mut self, directory: impl Into<std::path::PathBuf>) -> Self {
        self.directory = Some(directory.into());
        self
    }

    /// Set the region, for the providers that have one.
    ///
    /// ```
    /// # use moso_mail::{MailBackendKind, MailConfig};
    /// let _ = MailConfig::new("a@b.com", MailBackendKind::Ses)?.region("eu-central-1");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Set the sending domain, for the providers that need one.
    ///
    /// ```
    /// # use moso_mail::{MailBackendKind, MailConfig};
    /// let _ = MailConfig::new("a@b.com", MailBackendKind::Mailgun)?.domain("mg.example.com");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Turn the `/_mail` preview inbox on or off.
    ///
    /// ```
    /// # use moso_mail::{MailBackendKind, MailConfig};
    /// let config = MailConfig::new("a@b.com", MailBackendKind::Console)?.preview(false);
    /// assert!(!config.preview);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn preview(mut self, enabled: bool) -> Self {
        self.preview = enabled;
        self
    }

    /// Turn the suppression check on or off.
    ///
    /// Off only for a test that is exercising the suppression list itself.
    ///
    /// ```
    /// # use moso_mail::{MailBackendKind, MailConfig};
    /// let _ = MailConfig::new("a@b.com", MailBackendKind::Memory)?.suppression(false);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn suppression(mut self, enabled: bool) -> Self {
        self.suppression = enabled;
        self
    }

    /// Check for contradictions before anything tries to connect.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) naming the field and the fix:
    /// a remote backend with no `url`, a `file` backend with no `directory`,
    /// a zero timeout.
    ///
    /// ```
    /// # use moso_mail::{MailBackendKind, MailConfig};
    /// // A remote backend with nowhere to connect is caught at boot, not at
    /// // the first send.
    /// let config = MailConfig::new("a@b.com", MailBackendKind::Smtp)?;
    /// assert!(config.validate().is_err());
    /// assert!(config.url("smtp://localhost:1025?security=none").validate().is_ok());
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.backend.needs_url() && self.url.is_none() {
            return Err(crate::Error::config(format!(
                "`mail.url` is required for the `{}` backend — set `MAIL_URL` to the DSN or the \
                 API key, or choose `MAIL_BACKEND=console` for local development",
                self.backend.as_str(),
            )));
        }
        if self.backend == MailBackendKind::File && self.directory.is_none() {
            return Err(crate::Error::config(
                "`mail.directory` is required for the `file` backend — set `MAIL_DIRECTORY` to a \
                 writable path such as `target/mail`",
            ));
        }
        if self.backend == MailBackendKind::Mailgun && self.domain.is_none() {
            return Err(crate::Error::config(
                "`mail.domain` is required for the `mailgun` backend — set `MAIL_DOMAIN` to the \
                 sending domain you verified with Mailgun, e.g. `mg.example.com`",
            ));
        }
        if self.timeout.is_zero() {
            return Err(crate::Error::config(
                "`mail.timeout` is zero, which abandons every send before it starts — set it to \
                 a few seconds, e.g. `MAIL_TIMEOUT=30s`",
            ));
        }
        Ok(())
    }

    /// Warnings an operator should see at boot, but which are not failures.
    ///
    /// A production profile sending through the console silently sends
    /// nothing, and a production profile serving `/_mail` publishes every
    /// password-reset link it has ever generated. Neither is worth refusing to
    /// start over — an operator may genuinely mean it — and both are worth
    /// saying out loud.
    ///
    /// ```
    /// # use moso_mail::{MailBackendKind, MailConfig};
    /// let config = MailConfig::new("a@b.com", MailBackendKind::Console)?;
    /// assert_eq!(config.warnings(true).len(), 2);
    /// assert!(config.warnings(false).is_empty());
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn warnings(&self, production: bool) -> Vec<String> {
        let mut warnings = Vec::new();
        if !production {
            return warnings;
        }
        if self.backend.is_local() {
            warnings.push(format!(
                "mail: this profile sends through the `{}` backend, so no message leaves the \
                 process — set `MAIL_BACKEND` to `smtp` or a provider",
                self.backend.as_str(),
            ));
        }
        if self.preview {
            warnings.push(format!(
                "mail: the preview inbox is served at `{}` in a production profile, and it shows \
                 message bodies — set `MAIL_PREVIEW=false`",
                crate::preview::PREVIEW_PATH,
            ));
        }
        if self.redirect_to.is_some() {
            warnings.push(
                "mail: every message is redirected in a production profile, so no customer \
                 receives anything — unset `MAIL_REDIRECT_TO`"
                    .to_owned(),
            );
        }
        if !self.suppression {
            warnings.push(
                "mail: the suppression list is off, so bounced and complained addresses will be \
                 mailed again — unset `MAIL_SUPPRESSION=false`"
                    .to_owned(),
            );
        }
        warnings
    }

    /// Build the mailer this configuration describes.
    ///
    /// Applies the composition wrappers in the documented order: the base
    /// backend, then [`Redirecting`](crate::backend::Redirecting) when
    /// `redirect_to` is set, then
    /// [`Suppressing`](crate::backend::Suppressing) when a list is given.
    ///
    /// The order matters and is the documented one: suppression is the
    /// **outermost** wrapper, so it sees the message's real recipients and a
    /// staging deployment refuses exactly what production would refuse. Were
    /// it inside the redirect it would only ever check the staging inbox, and
    /// a suppression bug would first appear in production.
    ///
    /// # Errors
    ///
    /// Everything [`validate`](MailConfig::validate) reports, plus
    /// [`Error::Config`](crate::Error::Config) when the chosen backend's cargo
    /// feature is off — with the feature name in the message.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_mail::{MailBackendKind, MailConfig, Mailer, SuppressionList};
    /// let config = MailConfig::new("hello@shop.example", MailBackendKind::Console)?;
    /// let mailer: Arc<dyn Mailer> = config.build(None)?;
    /// assert_eq!(mailer.name(), "console");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    pub fn build(
        &self,
        suppression: Option<std::sync::Arc<dyn crate::SuppressionList>>,
    ) -> Result<std::sync::Arc<dyn Mailer>> {
        self.validate()?;

        let base: std::sync::Arc<dyn Mailer> = self.base_backend()?;

        let redirected: std::sync::Arc<dyn Mailer> = match &self.redirect_to {
            Some(to) => std::sync::Arc::new(crate::backend::Redirecting::new(base, to.clone())),
            None => base,
        };

        Ok(match (self.suppression, suppression) {
            (true, Some(list)) => {
                std::sync::Arc::new(crate::backend::Suppressing::new(redirected, list))
            }
            _ => redirected,
        })
    }

    /// The bare backend, before any wrapper.
    fn base_backend(&self) -> Result<std::sync::Arc<dyn Mailer>> {
        /// The message a backend whose feature is off produces.
        fn disabled(kind: MailBackendKind) -> crate::Error {
            crate::Error::config(format!(
                "the `{}` mail backend needs the `{}` cargo feature — add \
                 `moso-mail = {{ features = [\"{}\"] }}` to your `Cargo.toml`",
                kind.as_str(),
                kind.feature(),
                kind.feature(),
            ))
        }

        match self.backend {
            MailBackendKind::Console => {
                #[cfg(feature = "console")]
                {
                    Ok(std::sync::Arc::new(
                        crate::backend::ConsoleMailer::new()
                            .from(self.from.clone())
                            .timeout(self.timeout),
                    ))
                }
                #[cfg(not(feature = "console"))]
                Err(disabled(MailBackendKind::Console))
            }
            MailBackendKind::Memory => {
                #[cfg(feature = "memory")]
                {
                    let mailer = crate::backend::MemoryMailer::new().timeout(self.timeout);
                    mailer.set_from(Some(self.from.clone()));
                    Ok(std::sync::Arc::new(mailer))
                }
                #[cfg(not(feature = "memory"))]
                Err(disabled(MailBackendKind::Memory))
            }
            MailBackendKind::File => {
                #[cfg(feature = "file")]
                {
                    let directory = self
                        .directory
                        .clone()
                        .ok_or_else(|| crate::Error::config("`mail.directory` is required"))?;
                    Ok(std::sync::Arc::new(
                        crate::backend::FileMailer::new(directory)
                            .from(self.from.clone())
                            .timeout(self.timeout),
                    ))
                }
                #[cfg(not(feature = "file"))]
                Err(disabled(MailBackendKind::File))
            }
            MailBackendKind::Smtp => {
                #[cfg(feature = "mail-smtp")]
                {
                    let url = self
                        .url
                        .as_ref()
                        .ok_or_else(|| crate::Error::config("`mail.url` is required"))?;
                    Ok(std::sync::Arc::new(
                        crate::backend::SmtpMailer::from_url(url.expose())?
                            .from(self.from.clone())
                            .timeout(self.timeout),
                    ))
                }
                #[cfg(not(feature = "mail-smtp"))]
                Err(disabled(MailBackendKind::Smtp))
            }
            other => self.provider_backend(other, disabled),
        }
    }

    /// The REST provider backends, which share a constructor.
    ///
    /// With every provider feature off there is no `MailProvider` to build and
    /// only the error arm is reachable, which is why that arm consumes both
    /// parameters explicitly.
    fn provider_backend(
        &self,
        kind: MailBackendKind,
        disabled: fn(MailBackendKind) -> crate::Error,
    ) -> Result<std::sync::Arc<dyn Mailer>> {
        #[cfg(feature = "provider")]
        {
            use crate::backend::{MailProvider, ProviderMailer};

            let provider = match kind {
                #[cfg(feature = "mail-ses")]
                MailBackendKind::Ses => MailProvider::Ses,
                #[cfg(feature = "mail-sendgrid")]
                MailBackendKind::Sendgrid => MailProvider::Sendgrid,
                #[cfg(feature = "mail-postmark")]
                MailBackendKind::Postmark => MailProvider::Postmark,
                #[cfg(feature = "mail-resend")]
                MailBackendKind::Resend => MailProvider::Resend,
                #[cfg(feature = "mail-mailgun")]
                MailBackendKind::Mailgun => MailProvider::Mailgun,
                other => return Err(disabled(other)),
            };

            let key = self
                .url
                .clone()
                .ok_or_else(|| crate::Error::config("`mail.url` is required"))?;
            let mut mailer = ProviderMailer::new(provider, key)
                .from(self.from.clone())
                .timeout(self.timeout);
            if let Some(region) = &self.region {
                mailer = mailer.region(region.clone());
            }
            if let Some(domain) = &self.domain {
                mailer = mailer.domain(domain.clone());
            }
            Ok(std::sync::Arc::new(mailer))
        }

        #[cfg(not(feature = "provider"))]
        {
            let _ = &self;
            Err(disabled(kind))
        }
    }
}

/// Parse a `From` that may carry a display name.
///
/// `MAIL_FROM` is nearly always written as `Shop <hello@shop.example>`, which
/// is a *header* and not an address; splitting it here means an operator does
/// not have to know the difference.
fn parse_from(value: &str) -> Result<Address> {
    let value = value.trim();
    if let Some(open) = value.rfind('<')
        && let Some(close) = value.rfind('>')
        && close > open
    {
        let address = Address::new(&value[open + 1..close])?;
        let name = value[..open].trim().trim_matches('"').trim();
        return Ok(if name.is_empty() {
            address
        } else {
            address.with_name(name)
        });
    }
    Address::new(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SuppressionList as _;

    /// `MAIL_FROM` is nearly always a header, not a bare address.
    #[test]
    fn a_from_with_a_display_name_parses() {
        let config =
            MailConfig::new("Shop <hello@shop.example>", MailBackendKind::Console).expect("parses");
        assert_eq!(config.from.address(), "hello@shop.example");
        assert_eq!(config.from.name(), Some("Shop"));
        assert_eq!(config.from.to_header(), "Shop <hello@shop.example>");

        let quoted =
            MailConfig::new("\"Shop, Ltd\" <a@b.com>", MailBackendKind::Console).expect("parses");
        assert_eq!(quoted.from.name(), Some("Shop, Ltd"));
    }

    /// A `From` that is not an address at all fails at construction.
    #[test]
    fn a_from_that_is_not_an_address_is_refused() {
        assert!(MailConfig::new("not an address", MailBackendKind::Console).is_err());
        assert!(MailConfig::new("Shop <not an address>", MailBackendKind::Console).is_err());
    }

    /// Every combination the boot report should catch, caught.
    #[test]
    fn validation_catches_the_contradictions_it_is_for() {
        let smtp = MailConfig::new("a@b.com", MailBackendKind::Smtp).expect("valid");
        assert!(smtp.validate().is_err(), "no url");

        let file = MailConfig::new("a@b.com", MailBackendKind::File).expect("valid");
        assert!(file.validate().is_err(), "no directory");
        assert!(file.directory("target/mail").validate().is_ok());

        let mailgun = MailConfig::new("a@b.com", MailBackendKind::Mailgun)
            .expect("valid")
            .url("key");
        assert!(mailgun.validate().is_err(), "no domain");
        assert!(mailgun.domain("mg.example.com").validate().is_ok());

        let zero = MailConfig::new("a@b.com", MailBackendKind::Console)
            .expect("valid")
            .timeout(Duration::ZERO);
        assert!(zero.validate().is_err(), "zero timeout");
    }

    /// The message names the environment variable, because that is what the
    /// operator reading it can change.
    #[test]
    fn a_validation_failure_names_the_variable_and_the_fix() {
        let error = MailConfig::new("a@b.com", MailBackendKind::Ses)
            .expect("valid")
            .validate()
            .expect_err("no url");
        let text = error.to_string();
        assert!(text.contains("MAIL_URL"), "{text}");
        assert!(text.contains("MAIL_BACKEND=console"), "{text}");
    }

    /// The preview default follows the backend, so `moso dev` gets an inbox
    /// and a deployment does not.
    #[test]
    fn the_preview_is_on_for_a_local_backend_and_off_otherwise() {
        assert!(
            MailConfig::new("a@b.com", MailBackendKind::Console)
                .expect("valid")
                .preview
        );
        assert!(
            !MailConfig::new("a@b.com", MailBackendKind::Ses)
                .expect("valid")
                .preview
        );
    }

    /// A production profile that would silently send nothing says so.
    #[test]
    fn a_production_profile_warns_about_a_local_backend_and_the_inbox() {
        let warnings = MailConfig::new("a@b.com", MailBackendKind::Console)
            .expect("valid")
            .warnings(true);
        assert!(warnings.iter().any(|w| w.contains("no message leaves")));
        assert!(warnings.iter().any(|w| w.contains("preview inbox")));
    }

    /// Suppression is the outermost wrapper, so staging refuses exactly what
    /// production would refuse rather than only checking the staging inbox.
    ///
    /// Asserted behaviourally rather than by looking at the types, because the
    /// behaviour is what an operator depends on.
    #[tokio::test]
    async fn suppression_sees_the_real_recipient_even_when_everything_is_redirected() {
        let config = MailConfig::new("a@b.com", MailBackendKind::Console)
            .expect("valid")
            .redirect_to(Address::new("staging@shop.example").expect("valid"));

        let list = std::sync::Arc::new(crate::MemorySuppressionList::new());
        list.record(crate::Suppression::new(
            Address::new("customer@example.com").expect("valid"),
            crate::SuppressionReason::HardBounce,
        ))
        .await
        .expect("records");

        let mailer = config.build(Some(list)).expect("builds");

        // The suppressed customer is refused in staging exactly as it would be
        // in production, which is the point of running the check outermost.
        let error = mailer
            .send(&Ping(Address::new("customer@example.com").expect("valid")))
            .await
            .expect_err("suppressed");
        assert!(error.is_suppressed());

        // Anybody else still goes through — to the staging inbox.
        mailer
            .send(&Ping(Address::new("other@example.com").expect("valid")))
            .await
            .expect("not suppressed");
    }

    /// With no list there is nothing to suppress against, so a suppressed
    /// address is not consulted at all.
    #[tokio::test]
    async fn no_suppression_list_means_no_suppression_check() {
        MailConfig::new("a@b.com", MailBackendKind::Console)
            .expect("valid")
            .build(None)
            .expect("builds")
            .send(&Ping(Address::new("anyone@example.com").expect("valid")))
            .await
            .expect("sends");
    }

    /// Turning suppression off really turns it off, rather than keeping a
    /// wrapper that quietly still checks.
    #[tokio::test]
    async fn suppression_can_be_turned_off_for_a_test_of_the_list_itself() {
        let list = std::sync::Arc::new(crate::MemorySuppressionList::new());
        list.record(crate::Suppression::new(
            Address::new("bounced@example.com").expect("valid"),
            crate::SuppressionReason::HardBounce,
        ))
        .await
        .expect("records");

        MailConfig::new("a@b.com", MailBackendKind::Console)
            .expect("valid")
            .suppression(false)
            .build(Some(list))
            .expect("builds")
            .send(&Ping(Address::new("bounced@example.com").expect("valid")))
            .await
            .expect("the check is off");
    }

    /// A deadline that `build` dropped on the floor would be the original bug
    /// wearing a new hat, so it is asserted through behaviour: a server that
    /// accepts and never speaks must produce a timeout naming the backend the
    /// configuration chose.
    #[cfg(feature = "mail-smtp")]
    #[tokio::test]
    async fn the_configured_deadline_reaches_the_backend_that_is_built() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds a loopback port");
        let port = listener.local_addr().expect("has an address").port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let mailer = MailConfig::new("a@b.com", MailBackendKind::Smtp)
            .expect("valid")
            .url(format!("smtp://127.0.0.1:{port}?security=none"))
            .timeout(Duration::from_millis(150))
            .build(None)
            .expect("builds");

        let error = mailer
            .send(&Ping(Address::new("ada@example.com").expect("valid")))
            .await
            .expect_err("the deadline fires");
        assert!(matches!(error, crate::Error::Timeout { .. }), "{error}");
        assert_eq!(error.backend(), Some("smtp"));
    }

    /// The smallest message a composition test needs.
    struct Ping(Address);

    impl crate::Email for Ping {
        fn to(&self) -> Vec<Address> {
            vec![self.0.clone()]
        }
        fn subject(&self) -> Result<String> {
            Ok("ping".to_owned())
        }
        fn html(&self) -> Result<String> {
            Ok("<p>ping</p>".to_owned())
        }
        fn text(&self) -> Result<String> {
            Ok("ping".to_owned())
        }
    }

    /// Every name in `NAMES` parses, and every kind has a feature.
    #[test]
    fn every_documented_backend_name_round_trips() {
        for name in MailBackendKind::NAMES {
            let kind = MailBackendKind::parse(name).expect("a documented name parses");
            assert_eq!(kind.as_str(), *name);
            assert!(!kind.feature().is_empty());
        }
        assert_eq!(MailBackendKind::NAMES.len(), 9);
    }

    /// A backend whose cargo feature is off names the feature rather than
    /// failing with "unknown backend".
    #[test]
    fn a_backend_behind_a_disabled_feature_names_the_feature() {
        // `file` is off by default, so with default features this is the
        // disabled path and with `--all-features` it is the enabled one. Both
        // outcomes are correct; what must never happen is a panic or a
        // message that does not name the feature.
        let config = MailConfig::new("a@b.com", MailBackendKind::File)
            .expect("valid")
            .directory("target/mail");
        match config.build(None) {
            Ok(mailer) => assert_eq!(mailer.name(), "file"),
            Err(error) => assert!(
                error.to_string().contains("mail-file") || error.to_string().contains("`file`")
            ),
        }
    }
}
