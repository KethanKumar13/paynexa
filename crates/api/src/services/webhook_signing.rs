use hmac::{Hmac, Mac};
use sha2::Sha256;

/// `Paynexa-Signature` value: `t=<unix>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`.
pub fn signature_header(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("t={timestamp},v1={hex}")
}

#[cfg(test)]
mod tests {
    use super::signature_header;

    const BODY: &[u8] = br#"{"id":"evt_1"}"#;

    #[test]
    fn matches_an_independently_computed_hmac() {
        // Reference value computed with .NET HMACSHA256 over `1700000000.{"id":"evt_1"}`.
        assert_eq!(
            signature_header("whsec_test", 1_700_000_000, BODY),
            "t=1700000000,v1=c89214b5b5da833daed6f0b8c5bb6bd58cea9022bd80ccc78230f3942d632925"
        );
    }

    #[test]
    fn any_change_to_secret_timestamp_or_body_changes_the_signature() {
        let original = signature_header("whsec_test", 1_700_000_000, BODY);
        assert_ne!(
            original,
            signature_header("whsec_other", 1_700_000_000, BODY)
        );
        assert_ne!(
            original,
            signature_header("whsec_test", 1_700_000_001, BODY)
        );
        assert_ne!(
            original,
            signature_header("whsec_test", 1_700_000_000, br#"{"id":"evt_2"}"#)
        );
    }
}
