#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = "Moso's mail battery: a framework-owned `Mailer`, checked templates and a dev inbox."]
//!
//! Transactional mail is one of the few things every application sends and
//! almost nobody builds well. The failure modes are all operational — an
//! undefined template variable in a password reset, an HTML-only message that
//! lands in spam, a bounce loop that costs a sending domain its reputation, an
//! SMTP call inside a request handler that turns a provider outage into a 500
//! on signup — and none of them are visible until they happen in production.
//!
//! ```no_run
//! use moso_mail::{Address, Email, Mailer, Result};
//!
//! /// The message sent to a new account.
//! pub struct Welcome {
//!     /// Who signed up.
//!     pub to: Address,
//!     /// The link that verifies their address.
//!     pub verify_url: String,
//! }
//!
//! impl Email for Welcome {
//!     fn to(&self) -> Vec<Address> { vec![self.to.clone()] }
//!     fn subject(&self) -> Result<String> { Ok("Welcome".to_owned()) }
//!     fn html(&self) -> Result<String> { Ok(format!("<a href={:?}>verify</a>", self.verify_url)) }
//!     fn text(&self) -> Result<String> { Ok(format!("verify: {}", self.verify_url)) }
//! }
//!
//! async fn welcome(mailer: &dyn Mailer, message: &Welcome) -> Result<()> {
//!     mailer.send(message).await?;
//!     Ok(())
//! }
//! ```
//!
//! # The map
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`mod@message`] | [`Email`], [`Message`], [`RenderedEmail`], [`Address`], [`Attachment`], [`MessageId`] |
//! | [`mod@mime`] | RFC 5322 serialisation, shared by the `.eml` and SMTP backends |
//! | [`mod@mailer`] | [`Mailer`], [`MailCapabilities`] |
//! | [`mod@template`] | [`TemplateEngine`], [`Jinja`], [`html_to_text`] |
//! | [`mod@suppression`] | [`SuppressionList`], [`Suppression`], [`SuppressionReason`] |
//! | [`mod@webhook`] | [`WebhookVerifier`], [`WebhookEvent`], [`apply_events()`], [`SharedSecretVerifier`](webhook::SharedSecretVerifier), [`SnsVerifier`](webhook::SnsVerifier) |
//! | [`mod@backend`] | every shipped [`Mailer`], plus the composition wrappers |
//! | [`mod@preview`] | the `/_mail` development inbox |
//! | [`mod@config`] | [`MailConfig`] — backend choice as configuration |
//! | [`mod@deadline`] | the per-send deadline every backend enforces |
//! | [`mod@error`] | [`Error`], and what each variant becomes over HTTP |
//!
//! # Three decisions worth knowing before reading the code
//!
//! **There are two representations of a message.** [`Email`] is what an
//! application writes; [`RenderedEmail`] is what a backend sends. The second is
//! `Serialize + DeserializeOwned`, which is what lets a message become a
//! `moso-jobs` payload — so sending happens in a worker, with retries and a
//! dead-letter queue, **without either crate depending on the other**
//! (dependency rule 5, `xtask/allow/dep-edges.toml`).
//!
//! **The text part is not optional.** [`Email::text`] has no default that
//! returns an empty string. [`Message`] fills it in from the HTML with
//! [`html_to_text`] when it is not given, and a hand-written implementation
//! can do the same in one line, so the requirement costs an application
//! nothing and removes a whole class of deliverability problem. (`moso-macros`
//! ships no `#[derive(Email)]`; the four methods are written by hand.)
//!
//! **Suppression is composition, not a flag.** [`Suppressing`](backend::Suppressing)
//! wraps any [`Mailer`], so no backend can forget the check. Same for
//! [`Redirecting`](backend::Redirecting), which makes it structurally
//! impossible for a staging deployment to mail a real customer.
//!
//! # Cargo features
//!
//! | Feature | Default | What it adds |
//! | --- | --- | --- |
//! | `console` | yes | `backend::ConsoleMailer` and the `/_mail` inbox |
//! | `memory` | yes | `backend::MemoryMailer`, the test double |
//! | `file` | no | `backend::FileMailer` |
//! | `mail-smtp` | no | `backend::SmtpMailer` |
//! | `mail-ses`, `mail-sendgrid`, `mail-postmark`, `mail-resend`, `mail-mailgun` | no | `backend::ProviderMailer` and the matching webhook verifier |
//!
//! Code spans rather than links for the feature-gated names: a link to a type
//! that only exists under a cargo feature is a broken link in every build that
//! does not turn it on, and `rustdoc::broken_intra_doc_links` is `deny` across
//! this workspace.

pub mod backend;
pub mod config;
pub mod deadline;
pub mod error;
pub mod mailer;
pub mod message;
pub mod mime;
pub mod preview;
#[cfg(feature = "mail-ses")]
mod sigv4;
pub mod suppression;
pub mod template;
pub mod webhook;

pub use crate::config::{MailBackendKind, MailConfig};
pub use crate::error::{BoxError, Error, Result};
pub use crate::mailer::{MailCapabilities, Mailer};
pub use crate::message::{
    Address, Attachment, Disposition, Email, Message, MessageId, MessageKey, RenderedEmail,
};
pub use crate::suppression::{
    MemorySuppressionList, Suppression, SuppressionList, SuppressionReason, describe_reason,
};
pub use crate::template::{
    Jinja, Template, TemplateEngine, TemplateSource, html_to_text, render_with,
};
pub use crate::webhook::{WebhookEvent, WebhookEventKind, WebhookVerifier, apply_events};

/// The version of this crate, for `moso doctor` and the boot log.
///
/// ```
/// assert!(!moso_mail::VERSION.is_empty());
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything an application that sends mail imports.
///
/// ```no_run
/// use moso_mail::prelude::*;
///
/// async fn go(mailer: &dyn Mailer, message: &dyn Email) -> Result<MessageId> {
///     mailer.send(message).await
/// }
/// ```
pub mod prelude {
    pub use crate::{
        Address, Attachment, Email, Error, MailCapabilities, Mailer, Message, MessageId,
        MessageKey, RenderedEmail, Result, SuppressionList, SuppressionReason,
    };
}

#[cfg(test)]
mod tests {
    /// The public surface resolves from the crate root, so an application
    /// writes `moso_mail::Mailer` and not `moso_mail::mailer::Mailer`. A name
    /// that stops resolving here is a breaking change somebody made by
    /// accident.
    #[test]
    fn the_frozen_surface_resolves_from_the_root() {
        fn exists<T>() {}

        exists::<crate::Address>();
        exists::<crate::Attachment>();
        exists::<crate::Disposition>();
        exists::<crate::Error>();
        exists::<crate::MailCapabilities>();
        exists::<crate::MailConfig>();
        exists::<crate::MessageId>();
        exists::<crate::MessageKey>();
        exists::<crate::RenderedEmail>();
        exists::<crate::Suppression>();
        exists::<crate::SuppressionReason>();
        exists::<crate::Template>();
        exists::<crate::WebhookEvent>();
        exists::<crate::preview::PreviewItem>();

        fn dyn_compatible(
            _: &dyn crate::Mailer,
            _: &dyn crate::Email,
            _: &dyn crate::SuppressionList,
            _: &dyn crate::WebhookVerifier,
            _: &dyn crate::TemplateEngine,
            _: &dyn crate::preview::Inbox,
        ) {
        }
        let _ = dyn_compatible;
    }
}
