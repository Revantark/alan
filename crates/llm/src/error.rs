use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("transport error: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("{}", http_message(.status))]
    Http { status: u16, body: String },
    #[error("serialization error: {0}")]
    Serialization(#[source] serde_json::Error),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}

/// The body stays out of the message: its wording is the server's, so the
/// transport logs it instead.
fn http_message(status: &u16) -> String {
    let reason = match status {
        401 => "the provider rejected the credentials",
        402 => "the account is out of credits",
        404 => "the model or endpoint was not found",
        429 => "the provider is rate limiting requests; try again in a moment",
        500..=599 => "the provider had a server error; try again",
        _ => "the provider refused the request",
    };

    let status = reqwest::StatusCode::from_u16(*status)
        .map_or_else(|_| status.to_string(), |code| code.to_string());

    format!("{reason} (HTTP {status})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_errors_show_our_message_not_the_body() {
        let error = LlmError::Http {
            status: 401,
            body: r#"{"error":{"message":"key sk-abc for org_123 is revoked"}}"#.into(),
        };
        assert_eq!(
            error.to_string(),
            "the provider rejected the credentials (HTTP 401 Unauthorized)"
        );
    }

    #[test]
    fn http_errors_name_the_status_class() {
        let message = |status| {
            LlmError::Http {
                status,
                body: String::new(),
            }
            .to_string()
        };
        assert_eq!(
            message(503),
            "the provider had a server error; try again (HTTP 503 Service Unavailable)"
        );
        assert_eq!(
            message(418),
            "the provider refused the request (HTTP 418 I'm a teapot)"
        );
    }
}
