use reqwest::blocking::Client;
use serde::Deserialize;
use std::{error::Error, time::Duration};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, Clone)]
pub struct LlamaCppEmbedder {
    pub base_url: String,
    pub model: String,
    pub timeout: Duration,
}

impl LlamaCppEmbedder {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            timeout: Duration::from_secs(60),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/embeddings", self.base_url.trim_end_matches('/'))
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.trim().is_empty() {
            return Err("Cannot embed empty text.".into());
        }

        let client = Client::builder().timeout(self.timeout).build()?;

        let response = client
            .post(self.endpoint())
            .json(&serde_json::json!({
                "model": self.model,
                "input": text
            }))
            .send()?;

        let status = response.status();

        let body = response.text()?;

        if !status.is_success() {
            return Err(format!("Embedding server returned HTTP {}: {}", status, body).into());
        }

        let response: EmbeddingResponse = serde_json::from_str(&body)?;

        let embedding = response
            .data
            .first()
            .ok_or("Embedding server returned no embedding.")?
            .embedding
            .clone();

        if embedding.is_empty() {
            return Err("Embedding server returned an empty vector.".into());
        }

        Ok(embedding)
    }
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_embedding_response() -> Result<()> {
        let json = r#"
        {
            "data": [
                {
                    "embedding": [
                        0.1,
                        -0.2,
                        0.3,
                        0.4
                    ]
                }
            ]
        }
        "#;

        let response: EmbeddingResponse = serde_json::from_str(json)?;

        assert_eq!(response.data[0].embedding.len(), 4);

        assert_eq!(response.data[0].embedding[2], 0.3);

        Ok(())
    }
}
