//! ClawHub promotional model offers client (v2026.7.1 `openclaw promos`).
//!
//! Provider-side client for discovering and claiming promotional model
//! offers (auto-detected plans/billing). The CLI surface (`mylobster promos`)
//! is the CLI cluster's half; this module owns the HTTP contract with
//! bounded body reads.

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;

/// Default ClawHub host.
pub const CLAWHUB_DEFAULT_URL: &str = "https://clawhub.ai";

/// One promotional offer row.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PromoOffer {
    pub id: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub claimed: bool,
}

fn clawhub_base(base_url: Option<&str>) -> String {
    base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(CLAWHUB_DEFAULT_URL)
        .trim_end_matches('/')
        .to_string()
}

/// List promotional offers. Malformed responses are rejected as
/// provider-owned errors; bodies are bounded.
pub async fn list_promos(
    client: &Client,
    base_url: Option<&str>,
    token: Option<&str>,
) -> Result<Vec<PromoOffer>> {
    let base = clawhub_base(base_url);
    let mut req = client.get(format!("{}/api/promos", base));
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        req = req.header("Authorization", format!("Bearer {}", token));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("ClawHub promos list failed ({})", status);
    }
    let payload = super::read_json_bounded(
        resp,
        super::DEFAULT_PROVIDER_BODY_LIMIT_BYTES,
        "ClawHub promos",
    )
    .await?;
    let offers = payload
        .get("offers")
        .or_else(|| payload.get("promos"))
        .cloned()
        .unwrap_or(payload);
    serde_json::from_value(offers).map_err(|_| anyhow::anyhow!("ClawHub promos: malformed JSON response"))
}

/// Claim a promotional offer by id. Returns the updated offer.
pub async fn claim_promo(
    client: &Client,
    base_url: Option<&str>,
    token: &str,
    offer_id: &str,
) -> Result<PromoOffer> {
    let base = clawhub_base(base_url);
    let resp = client
        .post(format!("{}/api/promos/{}/claim", base, offer_id))
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("ClawHub promo claim failed ({})", status);
    }
    let payload = super::read_json_bounded(
        resp,
        super::DEFAULT_PROVIDER_BODY_LIMIT_BYTES,
        "ClawHub promo claim",
    )
    .await?;
    let offer = payload.get("offer").cloned().unwrap_or(payload);
    serde_json::from_value(offer)
        .map_err(|_| anyhow::anyhow!("ClawHub promo claim: malformed JSON response"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn lists_offers_from_wrapped_payload() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/promos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "offers": [{"id": "promo-1", "provider": "openai", "model": "gpt-5.6",
                            "claimed": false}]
            })))
            .mount(&server)
            .await;
        let client = Client::new();
        let offers = list_promos(&client, Some(&server.uri()), Some("tok")).await.unwrap();
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].id, "promo-1");
        assert!(!offers[0].claimed);

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(
            reqs[0].headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer tok"
        );
    }

    #[tokio::test]
    async fn claim_posts_to_offer_route() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/promos/promo-1/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "offer": {"id": "promo-1", "claimed": true}
            })))
            .mount(&server)
            .await;
        let client = Client::new();
        let offer = claim_promo(&client, Some(&server.uri()), "tok", "promo-1")
            .await
            .unwrap();
        assert!(offer.claimed);
    }

    #[tokio::test]
    async fn malformed_payload_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/promos"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;
        let client = Client::new();
        let err = list_promos(&client, Some(&server.uri()), None).await.unwrap_err();
        assert!(err.to_string().contains("malformed JSON"));
    }

    #[test]
    fn base_url_normalization() {
        assert_eq!(clawhub_base(None), CLAWHUB_DEFAULT_URL);
        assert_eq!(clawhub_base(Some("https://hub.example/")), "https://hub.example");
    }
}
