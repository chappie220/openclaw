//! `web_search`: looks things up through OpenRouter's web plugin or SearXNG,
//! for example a fictional character the user wants as the agent's persona.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::{ModelConfig, SearchConfig, SearchProvider};

pub struct Searcher {
    http: reqwest::Client,
    backend: Backend,
    max_results: usize,
}

enum Backend {
    OpenRouter {
        base_url: String,
        model: String,
        api_key: String,
    },
    Searxng {
        url: String,
    },
}

impl Searcher {
    /// `None` when search is turned off.
    pub fn new(config: &SearchConfig, model: &ModelConfig, api_key: &str) -> Result<Option<Self>> {
        let backend = match config.provider {
            SearchProvider::Off => return Ok(None),
            SearchProvider::Openrouter => Backend::OpenRouter {
                base_url: model.base_url.trim_end_matches('/').to_owned(),
                model: config.model.clone().unwrap_or_else(|| model.model.clone()),
                api_key: api_key.to_owned(),
            },
            SearchProvider::Searxng => Backend::Searxng {
                url: config
                    .searxng_url
                    .clone()
                    .filter(|u| !u.trim().is_empty())
                    .context("search.provider is \"searxng\" but search.searxng_url is not set")?
                    .trim_end_matches('/')
                    .to_owned(),
            },
        };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(model.request_timeout_secs))
            .build()
            .context("cannot build HTTP client for web search")?;
        Ok(Some(Self {
            http,
            backend,
            max_results: config.max_results.clamp(1, 10),
        }))
    }

    pub async fn search(&self, query: &str) -> Result<String> {
        let query = query.trim();
        if query.is_empty() {
            bail!("query is empty");
        }
        match &self.backend {
            Backend::OpenRouter {
                base_url,
                model,
                api_key,
            } => {
                let body = json!({
                    "model": model,
                    "messages": [{"role": "user", "content": format!(
                        "Search the web for: {query}\n\nReport the relevant facts found in the \
                         results, concisely and in the language of the query, citing each source URL."
                    )}],
                    "plugins": [{"id": "web", "max_results": self.max_results}],
                    "stream": false,
                });
                let response = self
                    .http
                    .post(format!("{base_url}/chat/completions"))
                    .bearer_auth(api_key)
                    .header("X-Title", "OpenClaw")
                    .json(&body)
                    .send()
                    .await
                    .context("search request failed")?;
                Ok(openrouter_results(&json_body(response).await?))
            }
            Backend::Searxng { url } => {
                let url = reqwest::Url::parse_with_params(
                    &format!("{url}/search"),
                    [("q", query), ("format", "json")],
                )
                .context("invalid search.searxng_url")?;
                let response = self
                    .http
                    .get(url)
                    .send()
                    .await
                    .context("search request failed")?;
                Ok(searxng_results(
                    &json_body(response).await?,
                    self.max_results,
                ))
            }
        }
    }
}

async fn json_body(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let text = response
        .text()
        .await
        .context("search response interrupted")?;
    let body: Option<Value> = serde_json::from_str(&text).ok();
    if !status.is_success() {
        let message = body
            .as_ref()
            .and_then(|v| v.pointer("/error/message").and_then(Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| text.chars().take(300).collect());
        bail!("search failed with HTTP {status}: {message}");
    }
    body.context("search response is not JSON")
}

/// The search model's summary followed by the cited pages.
fn openrouter_results(body: &Value) -> String {
    let message = body.pointer("/choices/0/message");
    let mut out = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let mut seen = Vec::new();
    for citation in message
        .and_then(|m| m.get("annotations"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("url_citation"))
    {
        let Some(url) = citation.get("url").and_then(Value::as_str) else {
            continue;
        };
        if seen.contains(&url) {
            continue;
        }
        if seen.is_empty() {
            out.push_str("\n\nSources:");
        }
        seen.push(url);
        let title = citation.get("title").and_then(Value::as_str).unwrap_or("");
        out.push_str(&format!("\n- {title} {url}"));
    }
    if out.is_empty() {
        "no results".into()
    } else {
        out
    }
}

fn searxng_results(body: &Value, max: usize) -> String {
    let results: Vec<String> = body
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(max)
        .map(|r| {
            let field = |k| r.get(k).and_then(Value::as_str).unwrap_or("").trim();
            format!(
                "- {}\n  {}\n  {}",
                field("title"),
                field("url"),
                field("content")
            )
        })
        .collect();
    if results.is_empty() {
        "no results".into()
    } else {
        results.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_summary_lists_each_cited_page_once() {
        let body = json!({"choices": [{"message": {
            "content": "孙悟空性格顽皮。",
            "annotations": [
                {"type": "url_citation", "url_citation": {"url": "https://a.example/wk", "title": "孙悟空"}},
                {"type": "url_citation", "url_citation": {"url": "https://a.example/wk", "title": "孙悟空"}},
                {"type": "url_citation", "url_citation": {"url": "https://b.example/x", "title": "B"}}
            ]
        }}]});
        assert_eq!(
            openrouter_results(&body),
            "孙悟空性格顽皮。\n\nSources:\n- 孙悟空 https://a.example/wk\n- B https://b.example/x"
        );
        assert_eq!(openrouter_results(&json!({"choices": []})), "no results");
    }

    #[test]
    fn searxng_results_are_capped() {
        let body = json!({"results": [
            {"title": "One", "url": "https://1.example", "content": "first"},
            {"title": "Two", "url": "https://2.example", "content": "second"}
        ]});
        assert_eq!(
            searxng_results(&body, 1),
            "- One\n  https://1.example\n  first"
        );
    }

    #[test]
    fn searxng_needs_a_url() {
        let config = SearchConfig {
            provider: SearchProvider::Searxng,
            ..SearchConfig::default()
        };
        let err = Searcher::new(&config, &ModelConfig::default(), "k")
            .err()
            .unwrap();
        assert!(err.to_string().contains("searxng_url"));
    }
}
