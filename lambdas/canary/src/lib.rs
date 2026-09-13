use lambda_runtime::Error;

const CURRENT_KEY: &str = "\"current\"";
const BODY_PREVIEW_CHARS: usize = 200;

pub fn healthy(status: u16, body: &str) -> bool {
    status == 200 && body.contains(CURRENT_KEY)
}

pub async fn probe(client: &reqwest::Client, url: &str) -> Result<(), Error> {
    let response = client.get(url).send().await?;
    let status = response.status().as_u16();
    let body = response.text().await?;

    if healthy(status, &body) {
        tracing::info!(status, "browse probe ok");
        return Ok(());
    }

    let preview: String = body.chars().take(BODY_PREVIEW_CHARS).collect();
    tracing::error!(status, body = %preview, "browse probe failed");
    Err(format!("browse probe failed with status {status}").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_200_carrying_the_current_key_is_healthy() {
        assert!(healthy(200, r#"{"current":null,"next":null}"#));
        assert!(healthy(200, r#"{"current":{"raffleId":"winter"},"next":null}"#));
        assert!(!healthy(200, r#"{"error":"internal error"}"#));
        assert!(!healthy(500, r#"{"current":null}"#));
        assert!(!healthy(200, ""));
    }
}
