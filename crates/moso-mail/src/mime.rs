//! RFC 5322 / MIME serialisation: one implementation, three consumers.
//!
// The two consumers named below are behind cargo features, so the intra-doc
// link only exists in a build that has them. Written as a `cfg_attr` pair
// rather than a plain code span so that the link is live on docs.rs (which
// builds every feature) without `cargo doc` on the default features failing on
// a target that is not there.
#![cfg_attr(
    feature = "file",
    doc = "[`FileMailer`](crate::backend::FileMailer) writes the bytes as `.eml`,"
)]
#![cfg_attr(
    not(feature = "file"),
    doc = "`FileMailer` (cargo feature `file`) writes the bytes as `.eml`,"
)]
#![cfg_attr(
    feature = "mail-smtp",
    doc = "[`SmtpMailer`](crate::backend::SmtpMailer) hands them to the transport, and"
)]
#![cfg_attr(
    not(feature = "mail-smtp"),
    doc = "`SmtpMailer` (cargo feature `mail-smtp`) hands them to the transport, and"
)]
//! the `/_mail` preview inbox offers them as a download. Writing this once
//! rather than three times is the reason the crate does not need a message
//! builder from a mail library.
//!
//! # The three rules that are easy to get wrong
//!
//! **`Bcc` is not a header.** Blind copies are recipients of the *envelope*,
//! not of the message; a `Bcc:` header in the transmitted bytes tells every
//! recipient who else got it. It is omitted here and carried separately.
//!
//! **Anything non-ASCII in a header is an encoded word.** A raw UTF-8 subject
//! is illegal in RFC 5322 and renders as mojibake in enough clients to matter.
//!
//! **Every header value is flattened.** A value containing CR or LF ends the
//! header and starts injecting whatever follows; that has to be impossible
//! rather than unlikely.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::{Address, Disposition, RenderedEmail};

/// The line ending RFC 5322 requires. Not `\n`.
const CRLF: &str = "\r\n";

/// The longest base64 line, in characters. RFC 2045 caps a line at 76.
const BASE64_LINE: usize = 76;

/// Serialise a rendered message into RFC 5322 bytes.
///
/// `message_id` is the `Message-ID` header's value, without angle brackets; the
/// caller owns it because it also has to correlate with the provider's own
/// identifier and with the idempotency key.
///
/// ```
/// # use moso_mail::{Address, Email, RenderedEmail, Result};
/// # struct Ping(Address);
/// # impl Email for Ping {
/// #     fn to(&self) -> Vec<Address> { vec![self.0.clone()] }
/// #     fn subject(&self) -> Result<String> { Ok("hi".to_owned()) }
/// #     fn html(&self) -> Result<String> { Ok("<p>hi</p>".to_owned()) }
/// #     fn text(&self) -> Result<String> { Ok("hi".to_owned()) }
/// # }
/// # let rendered = RenderedEmail::render(&Ping(Address::new("a@b.com")?))?;
/// # let bytes = moso_mail::mime::to_rfc5322(&rendered, "abc@moso.invalid");
/// assert!(bytes.starts_with(b"From:"));
/// # Ok::<(), moso_mail::Error>(())
/// ```
#[must_use]
pub fn to_rfc5322(message: &RenderedEmail, message_id: &str) -> Vec<u8> {
    let mut out = String::with_capacity(message.html.len() + message.text.len() + 1024);

    header(&mut out, "From", &message.from.to_header());
    header(&mut out, "To", &address_list(&message.to));
    if !message.cc.is_empty() {
        header(&mut out, "Cc", &address_list(&message.cc));
    }
    if let Some(reply_to) = &message.reply_to {
        header(&mut out, "Reply-To", &reply_to.to_header());
    }
    header(&mut out, "Subject", &encode_word(&message.subject));
    header(&mut out, "Date", &rfc2822_now());
    header(&mut out, "Message-ID", &format!("<{message_id}>"));
    header(&mut out, "MIME-Version", "1.0");

    // Application headers last, so a `List-Unsubscribe` is visible next to the
    // body rather than lost among the envelope headers. `Bcc` is dropped: it
    // is an envelope concern, and emitting it leaks the blind copies.
    for (name, value) in &message.headers {
        if name.eq_ignore_ascii_case("bcc")
            || name.eq_ignore_ascii_case("message-id")
            || name.eq_ignore_ascii_case("date")
            || name.eq_ignore_ascii_case("mime-version")
        {
            continue;
        }
        header(&mut out, name, &encode_word(value));
    }

    let (inline, attached): (Vec<_>, Vec<_>) = message
        .attachments
        .iter()
        .partition(|attachment| attachment.disposition() == Disposition::Inline);

    // The nesting is decided by what the message actually carries, because an
    // unnecessary `multipart/mixed` around a single part makes some clients
    // show an empty attachment.
    let alternative = boundary('a');
    let mut body = String::new();
    write_alternative(&mut body, message, &alternative);

    if inline.is_empty() && attached.is_empty() {
        out.push_str(&format!(
            "Content-Type: multipart/alternative; boundary=\"{alternative}\"{CRLF}{CRLF}"
        ));
        out.push_str(&body);
        return out.into_bytes();
    }

    // An inline image is referenced by the HTML, so it belongs in a
    // `multipart/related` with the alternative — not beside it.
    if !inline.is_empty() {
        let related = boundary('r');
        let mut related_body = String::new();
        related_body.push_str(&format!("--{related}{CRLF}"));
        related_body.push_str(&format!(
            "Content-Type: multipart/alternative; boundary=\"{alternative}\"{CRLF}{CRLF}"
        ));
        related_body.push_str(&body);
        for attachment in &inline {
            write_attachment(&mut related_body, attachment, &related);
        }
        related_body.push_str(&format!("--{related}--{CRLF}"));
        body = related_body;

        if attached.is_empty() {
            out.push_str(&format!(
                "Content-Type: multipart/related; boundary=\"{related}\"{CRLF}{CRLF}"
            ));
            out.push_str(&body);
            return out.into_bytes();
        }

        let mixed = boundary('m');
        out.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{mixed}\"{CRLF}{CRLF}"
        ));
        out.push_str(&format!("--{mixed}{CRLF}"));
        out.push_str(&format!(
            "Content-Type: multipart/related; boundary=\"{related}\"{CRLF}{CRLF}"
        ));
        out.push_str(&body);
        for attachment in &attached {
            write_attachment(&mut out, attachment, &mixed);
        }
        out.push_str(&format!("--{mixed}--{CRLF}"));
        return out.into_bytes();
    }

    let mixed = boundary('m');
    out.push_str(&format!(
        "Content-Type: multipart/mixed; boundary=\"{mixed}\"{CRLF}{CRLF}"
    ));
    out.push_str(&format!("--{mixed}{CRLF}"));
    out.push_str(&format!(
        "Content-Type: multipart/alternative; boundary=\"{alternative}\"{CRLF}{CRLF}"
    ));
    out.push_str(&body);
    for attachment in &attached {
        write_attachment(&mut out, attachment, &mixed);
    }
    out.push_str(&format!("--{mixed}--{CRLF}"));
    out.into_bytes()
}

/// The `text/plain` then `text/html` pair, in that order.
///
/// Order is load-bearing: a client picks the *last* part it understands, so
/// HTML must come second or every graphical client shows the text part.
fn write_alternative(out: &mut String, message: &RenderedEmail, boundary: &str) {
    for (media, content) in [("text/plain", &message.text), ("text/html", &message.html)] {
        out.push_str(&format!("--{boundary}{CRLF}"));
        out.push_str(&format!("Content-Type: {media}; charset=utf-8{CRLF}"));
        out.push_str(&format!(
            "Content-Transfer-Encoding: quoted-printable{CRLF}{CRLF}"
        ));
        out.push_str(&quoted_printable(content));
        out.push_str(CRLF);
    }
    out.push_str(&format!("--{boundary}--{CRLF}"));
}

/// One attachment part, base64 in 76-character lines.
fn write_attachment(out: &mut String, attachment: &crate::Attachment, boundary: &str) {
    out.push_str(&format!("--{boundary}{CRLF}"));
    out.push_str(&format!(
        "Content-Type: {}; name=\"{}\"{CRLF}",
        flatten(attachment.content_type()),
        flatten(attachment.filename()),
    ));
    out.push_str(&format!("Content-Transfer-Encoding: base64{CRLF}"));
    let disposition = match attachment.disposition() {
        Disposition::Inline => "inline",
        Disposition::Attachment => "attachment",
    };
    out.push_str(&format!(
        "Content-Disposition: {disposition}; filename=\"{}\"{CRLF}",
        flatten(attachment.filename()),
    ));
    if let Some(id) = attachment.content_id() {
        out.push_str(&format!("Content-ID: <{}>{CRLF}", flatten(id)));
    }
    out.push_str(CRLF);
    for chunk in STANDARD
        .encode(attachment.body())
        .as_bytes()
        .chunks(BASE64_LINE)
    {
        // `chunk` came from base64, which is ASCII, so this cannot fail.
        out.push_str(core::str::from_utf8(chunk).unwrap_or_default());
        out.push_str(CRLF);
    }
}

/// Write one header, flattening the value.
fn header(out: &mut String, name: &str, value: &str) {
    out.push_str(&flatten(name));
    out.push_str(": ");
    out.push_str(&flatten(value));
    out.push_str(CRLF);
}

/// Remove every CR and LF from a header value.
///
/// The one thing that makes header injection impossible rather than unlikely.
fn flatten(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c == '\r' || c == '\n' {
            // A stripped CRLF must not leave a double space behind it.
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Render a comma-separated address list.
fn address_list(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(Address::to_header)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Encode a header value as an RFC 2047 encoded word when it is not ASCII.
///
/// Base64 rather than quoted-printable: for a subject in a non-Latin script
/// quoted-printable is longer than the text it encodes.
fn encode_word(value: &str) -> String {
    if value.is_ascii() {
        return flatten(value);
    }
    format!("=?utf-8?B?{}?=", STANDARD.encode(flatten(value)))
}

/// Quoted-printable, with soft line breaks at 76 characters.
///
/// Written here rather than pulled in: the rules are three lines long, and the
/// alternative is a dependency on the critical path of every mail send.
fn quoted_printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut column = 0_usize;

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        column = 0;
        for byte in line.bytes() {
            // Printable ASCII except `=` goes through; everything else, and a
            // trailing space, is escaped.
            let escaped = match byte {
                b'=' => true,
                0x21..=0x7e => false,
                b' ' | b'\t' => false,
                _ => true,
            };
            let width = if escaped { 3 } else { 1 };
            if column + width > BASE64_LINE - 1 {
                out.push('=');
                out.push_str(CRLF);
                column = 0;
            }
            if escaped {
                out.push_str(&format!("={byte:02X}"));
            } else {
                out.push(char::from(byte));
            }
            column += width;
        }
        // A line ending in whitespace loses it in transit unless it is encoded.
        if out.ends_with(' ') {
            out.pop();
            out.push_str("=20");
        } else if out.ends_with('\t') {
            out.pop();
            out.push_str("=09");
        }
        out.push_str(CRLF);
    }
    let _ = column;

    // `split` produced one more element than there were newlines, so the last
    // CRLF is one this function added and the caller did not ask for.
    out.truncate(out.len().saturating_sub(CRLF.len()));
    out
}

/// A MIME boundary that cannot occur in the body.
///
/// 12 random bytes from the system generator, hex-encoded, behind a short
/// prefix. A boundary that collides with the content truncates the message and
/// a counter-based one collides across processes; the length is kept down so
/// that `Content-Type: multipart/alternative; boundary="…"` still fits in the
/// 78 characters RFC 5322 recommends for a line.
fn boundary(tag: char) -> String {
    use ring::rand::SecureRandom as _;

    let mut bytes = [0_u8; 12];
    // A failure here means the OS generator is unavailable, which is not
    // recoverable and not worth a `Result` on every call. The fallback is the
    // current time, which is unique enough for a boundary within one process.
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        bytes.copy_from_slice(&nanos.to_le_bytes()[..12]);
    }
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("=_m{tag}{hex}")
}

/// `Date`, in the RFC 2822 form mail requires.
fn rfc2822_now() -> String {
    chrono::Utc::now().to_rfc2822()
}

/// A `Message-ID` value: 16 random bytes at the sender's domain.
///
/// Every message needs a globally unique one; threading, deduplication and
/// most bounce correlation depend on it.
///
/// ```
/// let id = moso_mail::mime::new_message_id("shop.example");
/// assert!(id.ends_with("@shop.example"));
/// ```
#[must_use]
pub fn new_message_id(domain: &str) -> String {
    use ring::rand::SecureRandom as _;

    let mut bytes = [0_u8; 16];
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        bytes.copy_from_slice(&nanos.to_le_bytes());
    }
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let domain = if domain.is_empty() {
        "moso.invalid"
    } else {
        domain
    };
    format!("{hex}@{domain}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attachment, Email, Result};

    /// A message with whatever the test needs on it.
    struct Message {
        to: Vec<Address>,
        bcc: Vec<Address>,
        subject: String,
        attachments: Vec<Attachment>,
        headers: http::HeaderMap,
    }

    impl Message {
        fn new() -> Self {
            Self {
                to: vec![Address::new("ada@example.com").expect("valid")],
                bcc: Vec::new(),
                subject: "Hello".to_owned(),
                attachments: Vec::new(),
                headers: http::HeaderMap::new(),
            }
        }

        fn render(self) -> RenderedEmail {
            RenderedEmail::render(&self).expect("renders")
        }
    }

    impl Email for Message {
        fn to(&self) -> Vec<Address> {
            self.to.clone()
        }
        fn bcc(&self) -> Vec<Address> {
            self.bcc.clone()
        }
        fn subject(&self) -> Result<String> {
            Ok(self.subject.clone())
        }
        fn html(&self) -> Result<String> {
            Ok("<p>Hello</p>".to_owned())
        }
        fn text(&self) -> Result<String> {
            Ok("Hello".to_owned())
        }
        fn attachments(&self) -> Vec<Attachment> {
            self.attachments.clone()
        }
        fn headers(&self) -> http::HeaderMap {
            self.headers.clone()
        }
    }

    fn serialise(message: Message) -> String {
        String::from_utf8(to_rfc5322(&message.render(), "id@moso.invalid")).expect("utf-8")
    }

    /// A message with no files is one `multipart/alternative`, with the HTML
    /// second so a graphical client picks it.
    #[test]
    fn a_plain_message_is_one_alternative_with_html_last() {
        let text = serialise(Message::new());
        assert!(text.contains("Content-Type: multipart/alternative;"));
        assert!(!text.contains("multipart/mixed"));

        let plain = text.find("text/plain").expect("a text part");
        let html = text.find("text/html").expect("an html part");
        assert!(plain < html, "html must come last");
    }

    /// A `Bcc:` header tells every recipient who else got the message. It must
    /// never reach the wire.
    #[test]
    fn blind_copies_never_appear_in_the_headers() {
        let mut message = Message::new();
        message.bcc = vec![Address::new("secret@example.com").expect("valid")];
        let text = serialise(message);

        assert!(!text.contains("Bcc"));
        assert!(!text.contains("secret@example.com"));
    }

    /// An application that sets `Bcc`, `Date`, `Message-ID` or `MIME-Version`
    /// as a header must not get a second copy of an envelope header — a
    /// duplicated `Message-ID` breaks threading, and a `Bcc:` header leaks the
    /// blind copies whichever field it arrived in.
    #[test]
    fn an_application_header_cannot_duplicate_or_forge_an_envelope_header() {
        let mut message = Message::new();
        for (name, value) in [
            ("bcc", "secret@example.com"),
            ("date", "Thu, 1 Jan 1970 00:00:00 +0000"),
            ("message-id", "<forged@example.net>"),
            ("mime-version", "9.9"),
        ] {
            message.headers.insert(
                http::HeaderName::from_static(name),
                http::HeaderValue::from_str(value).expect("a legal value"),
            );
        }
        let text = serialise(message);

        let count = |prefix: &str| {
            text.lines()
                .filter(|line| line.to_ascii_lowercase().starts_with(prefix))
                .count()
        };
        assert_eq!(count("date:"), 1);
        assert_eq!(count("message-id:"), 1);
        assert_eq!(count("mime-version:"), 1);
        assert_eq!(count("bcc:"), 0);
        assert!(!text.contains("secret@example.com"));
        assert!(!text.contains("forged@example.net"));
        assert!(text.contains("MIME-Version: 1.0"));
    }

    /// Bulk mail is refused at render time without a `List-Unsubscribe`; this
    /// is the other half of that promise — the header an application supplied,
    /// and the one-click directive Moso added, both reach the wire.
    #[test]
    fn a_marketing_message_carries_its_unsubscribe_headers_to_the_wire() {
        struct Bulk;
        impl Email for Bulk {
            fn to(&self) -> Vec<Address> {
                vec![Address::new("ada@example.com").expect("valid")]
            }
            fn subject(&self) -> Result<String> {
                Ok("March newsletter".to_owned())
            }
            fn html(&self) -> Result<String> {
                Ok("<p>news</p>".to_owned())
            }
            fn text(&self) -> Result<String> {
                Ok("news".to_owned())
            }
            fn marketing(&self) -> bool {
                true
            }
            fn headers(&self) -> http::HeaderMap {
                let mut headers = http::HeaderMap::new();
                headers.insert(
                    "list-unsubscribe",
                    http::HeaderValue::from_static("<https://shop.example/u/abc>"),
                );
                headers
            }
        }

        let rendered = RenderedEmail::render(&Bulk).expect("renders");
        let text = String::from_utf8(to_rfc5322(&rendered, "id@moso.invalid")).expect("utf-8");

        assert!(
            text.contains("list-unsubscribe: <https://shop.example/u/abc>"),
            "{text}",
        );
        assert!(
            text.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click"),
            "{text}",
        );
    }

    /// An application header that carried a CRLF would inject the rest of the
    /// message. Flattening makes that impossible rather than unlikely.
    #[test]
    fn a_header_value_cannot_inject_another_header() {
        let mut message = Message::new();
        message.subject = "hi\r\nBcc: evil@example.net".to_owned();
        let text = serialise(message);

        let subject_line = text
            .lines()
            .find(|line| line.starts_with("Subject:"))
            .expect("a subject");
        assert_eq!(subject_line, "Subject: hi Bcc: evil@example.net");
        assert!(!text.contains("\r\nBcc:"));
    }

    /// A raw UTF-8 subject is illegal in RFC 5322 and renders as mojibake.
    #[test]
    fn a_non_ascii_subject_becomes_an_encoded_word() {
        let mut message = Message::new();
        message.subject = "Rapporto annuale — ordinato".to_owned();
        let text = serialise(message);

        assert!(text.contains("Subject: =?utf-8?B?"));
        assert!(!text.contains("—"));
    }

    /// Attachments nest inside a `multipart/mixed` and are base64 with the
    /// filename in both the type and the disposition.
    #[test]
    fn an_attachment_produces_a_mixed_part() {
        let mut message = Message::new();
        message.attachments = vec![Attachment::new(
            "invoice.pdf",
            "application/pdf",
            vec![0xff_u8; 200],
        )];
        let text = serialise(message);

        assert!(text.contains("Content-Type: multipart/mixed;"));
        assert!(text.contains("Content-Type: application/pdf; name=\"invoice.pdf\""));
        assert!(text.contains("Content-Disposition: attachment; filename=\"invoice.pdf\""));
        assert!(text.contains("Content-Transfer-Encoding: base64"));
        // Every line fits in the 78 characters RFC 5322 recommends — including
        // the `boundary=` parameter, which is why the boundary is short.
        let longest = text
            .lines()
            .max_by_key(|line| line.len())
            .unwrap_or_default();
        assert!(longest.len() <= 78, "{} chars: {longest}", longest.len());
    }

    /// An inline image is referenced by the HTML, so it belongs in a
    /// `multipart/related` with the alternative rather than beside it.
    #[test]
    fn an_inline_attachment_produces_a_related_part() {
        let mut message = Message::new();
        message.attachments = vec![Attachment::inline(
            "logo",
            "logo.png",
            "image/png",
            vec![1_u8, 2, 3],
        )];
        let text = serialise(message);

        assert!(text.contains("Content-Type: multipart/related;"));
        assert!(text.contains("Content-ID: <logo>"));
        assert!(!text.contains("multipart/mixed"));
    }

    /// Both kinds together: `mixed[ related[ alternative, inline ], attached ]`.
    #[test]
    fn inline_and_attached_together_nest_correctly() {
        let mut message = Message::new();
        message.attachments = vec![
            Attachment::inline("logo", "logo.png", "image/png", vec![1_u8]),
            Attachment::new("invoice.pdf", "application/pdf", vec![2_u8]),
        ];
        let text = serialise(message);

        let mixed = text.find("multipart/mixed").expect("mixed");
        let related = text.find("multipart/related").expect("related");
        let alternative = text.find("multipart/alternative").expect("alternative");
        assert!(mixed < related && related < alternative);
    }

    /// Every boundary must be unique, or a message with two levels truncates.
    #[test]
    fn boundaries_do_not_repeat() {
        let first = boundary('a');
        let second = boundary('a');
        assert_ne!(first, second);
        assert!(first.starts_with("=_ma"), "{first}");
    }

    /// A `Message-ID` is globally unique or threading breaks.
    #[test]
    fn message_ids_do_not_repeat() {
        assert_ne!(new_message_id("a.example"), new_message_id("a.example"));
        assert!(new_message_id("").ends_with("@moso.invalid"));
    }

    /// Quoted-printable escapes what it must and leaves the rest readable, so
    /// a text part is still legible in a raw `.eml`.
    #[test]
    fn quoted_printable_escapes_only_what_it_must() {
        assert_eq!(quoted_printable("Hello"), "Hello");
        assert_eq!(quoted_printable("a=b"), "a=3Db");
        assert_eq!(quoted_printable("café"), "caf=C3=A9");
        assert_eq!(quoted_printable("one\ntwo"), "one\r\ntwo");
    }

    /// A line ending in a space loses it in transit unless it is encoded.
    #[test]
    fn quoted_printable_protects_trailing_whitespace() {
        assert_eq!(quoted_printable("trailing "), "trailing=20");
    }

    /// A long line must be broken with a soft break, or the SMTP server folds
    /// it somewhere of its own choosing.
    #[test]
    fn quoted_printable_soft_wraps_long_lines() {
        let encoded = quoted_printable(&"x".repeat(200));
        assert!(encoded.lines().all(|line| line.len() <= 76), "{encoded}");
        assert!(encoded.contains("=\r\n"));
    }

    /// The transmitted bytes are CRLF-terminated throughout: a bare LF is a
    /// protocol error that some servers reject outright.
    #[test]
    fn every_line_ends_with_crlf() {
        let text = serialise(Message::new());
        assert!(!text.contains('\n') || text.matches('\n').count() == text.matches(CRLF).count());
    }
}
