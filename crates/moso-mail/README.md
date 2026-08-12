# moso-mail

Moso's mail battery: a framework-owned `Mailer` trait, checked templates, a browsable development
preview inbox, a suppression list and verified provider webhooks.

Part of [Moso](https://github.com/lowsbarrel/moso). See `docs/03-batteries/34-mail-storage-realtime.md`
for the design.

```rust,ignore
use moso_mail::prelude::*;

async fn welcome(mailer: &dyn Mailer, message: &dyn Email) -> Result<MessageId> {
    mailer.send(message).await
}
```

## Status

**Implemented.** No `todo!()` remains.

| Piece | State |
| --- | --- |
| `Mailer`, `Email`, `RenderedEmail` | ✅ the `RenderedEmail` job-payload seam included |
| `Message` | ✅ builds an `Email` from values; derives the text part from the HTML |
| Templates (`Jinja`, `minijinja`) | ✅ strict undefined, extension-driven autoescaping, `variables()` for the check a test performs |
| Send deadline | ✅ `deadline::within`, applied by every backend; `Error::Timeout` is a retryable 504 |
| `ConsoleMailer` + the `/_mail` inbox | ✅ index, HTML part, text part, raw `.eml`, clear — all sandboxed |
| `FileMailer`, `MemoryMailer` | ✅ `.eml` on disk; `sent`/`sent_count`/`sent_of`/`count_of`/`clear`/`fail_with`/`delay` for `app.mail()` |
| `SmtpMailer` | ✅ pooled, STARTTLS/implicit TLS, DSN parsing, unencrypted refused off-loopback |
| `ProviderMailer` | ✅ SES (SigV4 + raw MIME), SendGrid, Postmark, Resend, Mailgun |
| Webhook verification | ✅ Mailgun and Resend/Svix HMAC-SHA256, Postmark shared token, SendGrid ECDSA-P256, SES/SNS RSA over a **pinned** key |
| Suppression | ✅ `Suppressing` composition; an unsubscribe blocks marketing only |
| MIME | ✅ own writer: `multipart/alternative`, `related`, `mixed`, quoted-printable, RFC 2047, no `Bcc` on the wire |

`moso-macros` ships **no** `#[derive(Email)]`. A message is written as four trait methods or built
with `Message`, and the variable check a derive would do at compile time is
`TemplateEngine::variables` compared against a context's keys in a test.

## Licence

MIT — see the root [`LICENSE`](../../LICENSE).
