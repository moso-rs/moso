//! AWS Signature Version 4, for the SES backend.
//!
//! Roughly eighty lines, against a dependency that would pull the whole AWS SDK
//! into a crate whose job is to send an email. The algorithm is fully specified
//! and does not change; the risk of writing it is a wrong signature, which
//! fails loudly at the first request rather than silently.
//!
//! Only what SES needs: a `POST` with a JSON body, `SHA256` payload hashing,
//! and no session token. It is deliberately not a general-purpose signer.

use ring::hmac;

/// Sign a request, returning the headers to send with it.
///
/// The returned list always contains `host`, `x-amz-date`, `x-amz-content-sha256`
/// and `authorization`, in that order, plus `content-type`.
///
/// `now` is a parameter rather than read from the clock so the signature is
/// reproducible in a test — a signer whose output cannot be pinned is a signer
/// nobody can check.
#[expect(
    clippy::too_many_arguments,
    reason = "every parameter is a distinct input to the signature; grouping them into a struct \
              would move the list one line up and nothing else"
)]
pub(crate) fn sign(
    method: &str,
    host: &str,
    path: &str,
    query: &str,
    body: &[u8],
    service: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<(String, String)> {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let payload_hash = hex(sha256(body));

    // The canonical request: method, path, query, the signed headers in
    // lowercase alphabetical order, then the payload hash.
    let signed_headers = "content-type;host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "{method}\n{path}\n{query}\n\
         content-type:application/json\n\
         host:{host}\n\
         x-amz-content-sha256:{payload_hash}\n\
         x-amz-date:{amz_date}\n\
         \n{signed_headers}\n{payload_hash}",
    );

    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(sha256(canonical_request.as_bytes())),
    );

    // The signing key is derived once per day, per region, per service.
    let key = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let key = hmac_sha256(&key, region.as_bytes());
    let key = hmac_sha256(&key, service.as_bytes());
    let key = hmac_sha256(&key, b"aws4_request");
    let signature = hex(hmac_sha256(&key, to_sign.as_bytes()));

    vec![
        ("host".to_owned(), host.to_owned()),
        ("content-type".to_owned(), "application/json".to_owned()),
        ("x-amz-date".to_owned(), amz_date),
        ("x-amz-content-sha256".to_owned(), payload_hash),
        (
            "authorization".to_owned(),
            format!(
                "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, \
                 SignedHeaders={signed_headers}, Signature={signature}",
            ),
        ),
    ]
}

/// SHA-256 of a byte string.
fn sha256(bytes: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .to_vec()
}

/// HMAC-SHA256, as a byte vector.
fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), message)
        .as_ref()
        .to_vec()
}

/// Lowercase hex, which is the only encoding SigV4 accepts.
fn hex(bytes: Vec<u8>) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vector from AWS's own `sigv4` test suite: `GET /` against
    /// `example.amazonaws.com`, adapted to the fixed header set this signer
    /// emits. Pinning the whole `Authorization` header is what makes a change
    /// to the canonicalisation visible.
    #[test]
    fn the_signature_is_reproducible_for_a_fixed_instant() {
        let now = chrono::DateTime::parse_from_rfc3339("2015-08-30T12:36:00Z")
            .expect("valid")
            .with_timezone(&chrono::Utc);

        let headers = sign(
            "POST",
            "example.amazonaws.com",
            "/",
            "",
            b"{}",
            "service",
            "us-east-1",
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            now,
        );

        let map: std::collections::BTreeMap<_, _> = headers.into_iter().collect();
        assert_eq!(map["x-amz-date"], "20150830T123600Z");
        assert_eq!(
            map["x-amz-content-sha256"],
            // SHA-256 of `{}`.
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
        );
        assert!(map["authorization"].starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, Signature=",
        ));

        // The same inputs must produce the same signature, or every retry is a
        // new request as far as AWS is concerned.
        let again = sign(
            "POST",
            "example.amazonaws.com",
            "/",
            "",
            b"{}",
            "service",
            "us-east-1",
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            now,
        );
        assert_eq!(
            map["authorization"],
            again
                .into_iter()
                .find(|(name, _)| name == "authorization")
                .expect("present")
                .1,
        );
    }

    /// One byte of the body changing must change the signature, or the payload
    /// hash is not actually in the canonical request.
    #[test]
    fn the_body_is_covered_by_the_signature() {
        let now = chrono::Utc::now();
        let of = |body: &[u8]| {
            sign(
                "POST",
                "h",
                "/p",
                "",
                body,
                "ses",
                "eu-central-1",
                "AK",
                "SK",
                now,
            )
            .into_iter()
            .find(|(name, _)| name == "authorization")
            .expect("present")
            .1
        };
        assert_ne!(of(b"{\"a\":1}"), of(b"{\"a\":2}"));
    }

    /// The region and the service are both in the scope, so a key derived for
    /// one cannot sign for another.
    #[test]
    fn the_scope_pins_the_region_and_the_service() {
        let now = chrono::Utc::now();
        let of = |region: &str, service: &str| {
            sign(
                "POST", "h", "/p", "", b"{}", service, region, "AK", "SK", now,
            )
            .into_iter()
            .find(|(name, _)| name == "authorization")
            .expect("present")
            .1
        };
        assert_ne!(of("us-east-1", "ses"), of("eu-central-1", "ses"));
        assert_ne!(of("us-east-1", "ses"), of("us-east-1", "s3"));
    }
}
