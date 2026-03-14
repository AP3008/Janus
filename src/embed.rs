use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Wrapper around fastembed TextEmbedding for generating embeddings
pub struct Embedder {
    model: Arc<Mutex<TextEmbedding>>,
}

impl Embedder {
    /// Create a new embedder with BGE-small-en-v1.5 (384-dim)
    pub fn new() -> anyhow::Result<Self> {
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(true),
        )?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
        })
    }

    /// Embed a single text string, returns 384-dimensional vector
    pub async fn embed_one(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let model = self.model.clone();
        let text = text.to_string();
        let result = tokio::task::spawn_blocking(move || {
            let mut model = model.blocking_lock();
            model.embed(vec![text], None)
        })
        .await??;

        result
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No embedding returned"))
    }
}
