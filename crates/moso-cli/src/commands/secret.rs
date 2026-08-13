//! `moso config --generate-secret` — one value, straight from the kernel.
//!
//! # Why this is thirty lines of reading a file
//!
//! A signing key is only as good as the entropy behind it, and the only source
//! this CLI is willing to trust is the one the operating system already runs:
//! `getrandom(2)` behind `/dev/urandom` on Unix, and `BCryptGenRandom` behind
//! .NET's `RandomNumberGenerator` on Windows. Nothing here mixes, stretches,
//! seeds or hashes anything — the bytes the kernel hands over are the bytes
//! that get printed. `AGENTS.md` puts it as a rule: never implement a
//! cryptographic primitive, and take security-relevant randomness only from the
//! OS CSPRNG.
//!
//! The two readers are tried in order and neither is a *fallback* in the usual
//! sense — they are the same guarantee reached through two different platform
//! interfaces. When both are unreachable the command fails and names
//! `openssl rand`; it never produces a weaker value to keep going, because a
//! secret nobody knows is weak is worse than no secret at all.
//!
//! ```text
//! /dev/urandom            → N bytes, exactly
//!   └─ absent (Windows)   → powershell RandomNumberGenerator → hex → N bytes
//!        └─ absent        → an environment error naming `openssl rand -base64 32`
//! ```
//!
//! Neither reader is behind a `#[cfg]`. A platform-gated branch is a branch
//! that compiles on one machine and is discovered to be broken on another, and
//! the cost of compiling both everywhere is one unused function.
//!
//! # Where the output goes
//!
//! The encoded secret goes to standard output, alone, so
//! `moso config --generate-secret | pbcopy` does the obvious thing. The
//! reminder that it must not be committed goes to standard **error**, which is
//! why it is written here rather than through [`Ui`] — the point of the line is
//! that it must not end up inside whatever the caller redirected stdout into.
//! Nothing is written to a file: `--out` is refused at the command line.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use crate::cli::{ConfigArgs, SecretFormat};
use crate::exit::{CliError, Outcome};
use crate::ui::Ui;

/// What to run when `/dev/urandom` is not there.
///
/// `Create()` and `GetBytes` rather than the newer static `Fill`, because
/// Windows PowerShell 5.1 ships on .NET Framework and does not have `Fill`.
/// The bytes come back as hexadecimal: one unambiguous encoding to parse, with
/// no padding and no alphabet to get wrong.
const POWERSHELL_SCRIPT: &str = "\
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create();\
$bytes = New-Object byte[] COUNT;\
$rng.GetBytes($bytes);\
[System.BitConverter]::ToString($bytes).Replace('-','')";

/// Run `moso config --generate-secret`.
///
/// # Errors
/// [`Fault::Environment`](crate::exit::Fault::Environment) when no operating
/// system random number generator can be reached — the one failure this command
/// has, and the one case where producing something anyway would be worse than
/// producing nothing.
pub fn run(ui: &Ui, args: &ConfigArgs) -> Outcome<()> {
    // The parser caps `--bytes` at 1024, so this holds on every target Rust
    // supports; it is a `?` and not an `expect` because a secret is the last
    // place to learn what an assumption was worth.
    let count = usize::try_from(args.bytes)
        .map_err(|_| CliError::usage("--bytes does not fit this platform's usize"))?;

    let bytes = random_bytes(count)?;
    let encoded = match args.format {
        SecretFormat::Base64 => base64(&bytes),
        SecretFormat::Hex => hex(&bytes),
    };

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": true,
            "secret": encoded,
            "bytes": count,
            "format": args.format.as_str(),
        }));
        return Ok(());
    }

    ui.emit_raw(&encoded);
    remind(count);
    Ok(())
}

/// The one line that says what to do with what was just printed.
///
/// Deliberately on stderr and deliberately not through [`Ui`]: a redirect of
/// standard output must capture the secret and nothing else, and `--quiet` must
/// not be able to silence the warning that comes with a credential.
fn remind(count: usize) {
    let mut stderr = std::io::stderr();
    let _ = writeln!(
        stderr,
        "{count} bytes from the OS random number generator — keep it out of git: \
         put it in .env or your platform's secret store"
    );
}

// ---------------------------------------------------------------------------
// The random number generator
// ---------------------------------------------------------------------------

/// Read `count` bytes from the operating system's random number generator.
///
/// # Errors
/// [`Fault::Environment`](crate::exit::Fault::Environment) when neither
/// interface answers, naming a command that will.
pub fn random_bytes(count: usize) -> Outcome<Vec<u8>> {
    if let Some(bytes) = from_urandom(count) {
        return Ok(bytes);
    }
    if let Some(bytes) = from_powershell(count) {
        return Ok(bytes);
    }
    Err(
        CliError::environment("could not read the operating system's random number generator")
            .with_help("generate one another way, e.g. `openssl rand -base64 32`"),
    )
}

/// The Unix reader.
///
/// `read_exact`, so a short read is a failure rather than a secret with fewer
/// bytes of entropy than it claims.
fn from_urandom(count: usize) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open("/dev/urandom").ok()?;
    let mut buffer = vec![0_u8; count];
    file.read_exact(&mut buffer).ok()?;
    Some(buffer)
}

/// The Windows reader.
///
/// Spawned rather than linked because reaching `BCryptGenRandom` directly needs
/// `unsafe` and a platform crate, and this crate forbids the first and has no
/// business acquiring the second to print one value.
fn from_powershell(count: usize) -> Option<Vec<u8>> {
    let script = POWERSHELL_SCRIPT.replace("COUNT", &count.to_string());
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let bytes = unhex(text.trim())?;
    (bytes.len() == count).then_some(bytes)
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

/// The standard base64 alphabet, RFC 4648 §4.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode as standard base64, with padding.
///
/// Written out rather than pulled in: it is twenty lines of table lookup with
/// no security properties of its own, and `moso-cli` is deliberately a five
/// dependency binary.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).copied().map_or(0, u32::from);
        let third = chunk.get(2).copied().map_or(0, u32::from);
        let packed = (first << 16) | (second << 8) | third;

        out.push(ALPHABET[(packed >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(packed >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(packed >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[packed as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Encode as lower-case hexadecimal.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Decode hexadecimal, in either case, returning `None` on anything else.
fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digits: Vec<u8> = text.bytes().collect();
    digits
        .chunks(2)
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the encodings ─────────────────────────────────────────────────────

    #[test]
    fn base64_matches_the_rfc_4648_test_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "from {input:?}");
        }
    }

    #[test]
    fn base64_covers_the_whole_alphabet_including_the_two_that_are_not_letters() {
        // 0xfb 0xff encodes to `+/`, the two characters a hand-written table
        // most often gets wrong or silently swaps for the URL-safe pair.
        assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
        assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
    }

    #[test]
    fn hex_is_lower_case_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn hex_round_trips_through_the_decoder_in_either_case() {
        let bytes = vec![0x00, 0x7f, 0x80, 0xff, 0x10];
        assert_eq!(unhex(&hex(&bytes)), Some(bytes.clone()));
        assert_eq!(unhex("007F80FF10"), Some(bytes));
    }

    #[test]
    fn a_malformed_hex_string_decodes_to_nothing_rather_than_to_zeroes() {
        assert_eq!(unhex("abc"), None, "an odd length is not a byte string");
        assert_eq!(unhex("zz"), None, "`z` is not a hex digit");
    }

    // ── the generator ─────────────────────────────────────────────────────

    #[test]
    fn the_generator_returns_exactly_the_requested_number_of_bytes() {
        let bytes = random_bytes(32).expect("this machine has an OS CSPRNG");
        assert_eq!(bytes.len(), 32);
        assert_eq!(base64(&bytes).len(), 44, "32 bytes is 44 base64 characters");
        assert_eq!(hex(&bytes).len(), 64);
    }

    #[test]
    fn two_secrets_are_not_the_same_secret() {
        // The failure this catches is a generator that returns a buffer it
        // never filled, which is indistinguishable from a working one until you
        // ask for a second value.
        let first = random_bytes(32).expect("an OS CSPRNG");
        let second = random_bytes(32).expect("an OS CSPRNG");
        assert_ne!(first, second);
        assert!(
            first.iter().any(|byte| *byte != 0),
            "an all-zero secret means the buffer was never written"
        );
    }
}
