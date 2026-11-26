//! Model discovery for the "add model" flow.
//!
//! Every OpenAI-compatible endpoint answers `GET {base}/models` with the list
//! of ids it serves, and some add context/output limits. The shapes differ, so
//! the parser accepts the common spellings instead of pinning one vendor, and
//! anything it does not recognise is left unknown rather than guessed.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::error::{HarnessError, Result};

/// One model a provider advertises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogModel {
    /// Id to send in requests.
    pub id: String,
    /// Display name, when the provider gives one that differs from the id.
    pub name: Option<String>,
    /// Context window; `0` when the provider does not report it.
    pub context_window: u64,
    /// Output cap, when the provider reports it.
    pub max_tokens: Option<u32>,
}

/// Ask `{base_url}/models` what the endpoint serves.
pub async fn fetch_catalog(base_url: &str, api_key: &str) -> Result<Vec<CatalogModel>> {
    let url = format!("{}/models", base_url.trim().trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        // Discovery is interactive: the user is watching a spinner, so a silent
        // endpoint must fail rather than hang the flow.
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| HarnessError::Config(format!("http client: {e}")))?;
    let mut request = client.get(&url);
    if !api_key.trim().is_empty() {
        request = request.bearer_auth(api_key.trim());
    }
    let response = request
        .send()
        .await
        .map_err(|e| HarnessError::Config(format!("cannot reach {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(HarnessError::Config(format!(
            "{url} returned HTTP {}",
            response.status()
        )));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| HarnessError::Config(format!("{url} did not return JSON: {e}")))?;
    // An empty list is not a failure: many local servers only take a model id
    // the caller already knows, and the flow falls back to typing one.
    Ok(parse_catalog(&body))
}

/// Pull the model list out of whatever envelope the provider uses:
/// `{"data": [...]}` (OpenAI), `{"models": [...]}` (Ollama and friends), a bare
/// array, or the same shapes under an extra `result`/`body` wrapper.
pub fn parse_catalog(body: &Value) -> Vec<CatalogModel> {
    let list = body
        .get("data")
        .or_else(|| body.get("models"))
        .or_else(|| body.get("result"))
        .or_else(|| body.get("body"))
        .unwrap_or(body);
    let Some(entries) = list.as_array() else {
        // A single model object is a valid answer too.
        return entry(list).into_iter().collect();
    };
    let mut by_id: BTreeMap<String, CatalogModel> = BTreeMap::new();
    for value in entries {
        if let Some(model) = entry(value) {
            by_id.insert(model.id.clone(), model);
        }
    }
    by_id.into_values().collect()
}

fn entry(value: &Value) -> Option<CatalogModel> {
    let id = value
        .get("id")
        .or_else(|| value.get("model"))
        .or_else(|| value.get("name"))
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    if id.is_empty() {
        return None;
    }
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != id)
        .map(str::to_string);
    Some(CatalogModel {
        id,
        name,
        context_window: number(value, CONTEXT_KEYS).unwrap_or(0),
        max_tokens: number(value, MAX_TOKENS_KEYS)
            .filter(|value| *value > 0)
            .map(|value| value.min(u32::MAX as u64) as u32),
    })
}

/// Spellings seen in the wild for the context window, most specific first.
const CONTEXT_KEYS: [&str; 8] = [
    "context_length",
    "context_window",
    "max_context_length",
    "max_model_len",
    "input_token_limit",
    "context_size",
    "max_input_tokens",
    "n_ctx",
];

const MAX_TOKENS_KEYS: [&str; 6] = [
    "max_completion_tokens",
    "max_output_tokens",
    "max_tokens",
    "output_token_limit",
    "max_generated_tokens",
    "max_new_tokens",
];

/// Read one of `keys` at the top level, or inside `top_provider` (OpenRouter
/// nests the limits there).
fn number<const N: usize>(value: &Value, keys: [&str; N]) -> Option<u64> {
    for key in keys {
        if let Some(found) = value.get(key).and_then(Value::as_u64) {
            return Some(found);
        }
        if let Some(found) = value
            .get("top_provider")
            .and_then(|nested| nested.get(key))
            .and_then(Value::as_u64)
        {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_style_ids_are_read_and_sorted() {
        let models = parse_catalog(&json!({
            "object": "list",
            "data": [
                { "id": "gpt-4o", "object": "model" },
                { "id": "gpt-4o-mini", "object": "model" }
            ]
        }));
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["gpt-4o", "gpt-4o-mini"]
        );
        assert_eq!(models[0].context_window, 0, "unknown stays unknown");
        assert_eq!(models[0].max_tokens, None);
    }

    #[test]
    fn context_and_output_limits_are_picked_up() {
        let models = parse_catalog(&json!({
            "data": [
                {
                    "id": "openai/gpt-5",
                    "name": "GPT-5",
                    "context_length": 400000,
                    "top_provider": { "max_completion_tokens": 128000 }
                },
                { "id": "ollama/qwen", "context_window": 32768, "max_tokens": 8192 }
            ]
        }));
        let gpt = models.iter().find(|m| m.id == "openai/gpt-5").unwrap();
        assert_eq!(gpt.name.as_deref(), Some("GPT-5"));
        assert_eq!(gpt.context_window, 400_000);
        assert_eq!(gpt.max_tokens, Some(128_000));
        let qwen = models.iter().find(|m| m.id == "ollama/qwen").unwrap();
        assert_eq!(qwen.context_window, 32_768);
        assert_eq!(qwen.max_tokens, Some(8_192));
    }

    #[test]
    fn ollama_style_and_bare_arrays_work() {
        let models = parse_catalog(&json!({ "models": [{ "name": "llama3:8b" }] }));
        assert_eq!(models[0].id, "llama3:8b");
        // ollama reports the same string twice; the duplicate name is dropped.
        assert_eq!(models[0].name, None);

        let models = parse_catalog(&json!([{ "model": "custom/model" }]));
        assert_eq!(models[0].id, "custom/model");
    }

    #[test]
    fn duplicates_collapse_and_junk_is_skipped() {
        let models = parse_catalog(&json!({
            "data": [
                { "id": "b" },
                { "id": "a" },
                { "id": "b", "context_length": 1000 },
                { "id": "   " },
                { "nope": 1 },
                "not an object"
            ]
        }));
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(models[1].context_window, 1_000, "the later duplicate wins");
    }
}
