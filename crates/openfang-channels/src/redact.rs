//! Shared redaction helpers for channel adapters.

/// Strip the request URL from a `reqwest::Error` before it is logged or
/// propagated. Channel credentials are frequently embedded in request URLs,
/// so retaining the URL in the error can leak them through logs or API errors.
pub(crate) fn redact_reqwest_error(error: reqwest::Error) -> reqwest::Error {
    error.without_url()
}

#[cfg(test)]
mod tests {
    use super::redact_reqwest_error;

    /// Connection-level errors can retain the full request URL, including a
    /// credential embedded in its path. Redaction must remove both the URL and
    /// the credential from the rendered error.
    #[tokio::test]
    async fn strips_credentials_from_attached_url() {
        let token = "123456789:AAFakeTokenForTestingOnly-doNotUse";
        let bad_url = format!("https://{{{{hostname}}}}/bot{token}/getUpdates");

        let error = reqwest::Client::new()
            .get(&bad_url)
            .send()
            .await
            .expect_err("malformed hostname must fail before any network I/O");

        assert!(
            error.url().is_some(),
            "test assumption broken: reqwest stopped attaching URLs to builder errors"
        );
        assert!(format!("{error}").contains(token));

        let redacted = redact_reqwest_error(error);
        assert!(
            !format!("{redacted}").contains(token),
            "redacted error still leaks the credential"
        );
        assert!(redacted.url().is_none());
    }
}
