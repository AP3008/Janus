use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct JanusConfig {
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    #[serde(default)]
    pub pipeline: PipelineConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub pricing: PricingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_upstream_url")]
    pub upstream_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PipelineConfig {
    #[serde(default = "default_true")]
    pub tool_dedup: bool,
    #[serde(default = "default_true")]
    pub regex_structural: bool,
    #[serde(default = "default_true")]
    pub ast_pruning: bool,
    #[serde(default = "default_min_lines_for_ast")]
    pub min_lines_for_ast: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_redis_url")]
    pub redis_url: String,
    #[serde(default = "default_similarity_cutoff")]
    pub similarity_cutoff: f64,
    #[serde(default = "default_ttl")]
    pub ttl_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PricingConfig {
    #[serde(default = "default_input_cost")]
    pub input_cost_per_1k: f64,
}

// Default value functions
fn default_server() -> ServerConfig {
    ServerConfig {
        listen: default_listen(),
        upstream_url: default_upstream_url(),
    }
}

fn default_listen() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_upstream_url() -> String {
    "https://api.anthropic.com".to_string()
}

fn default_true() -> bool {
    true
}

fn default_min_lines_for_ast() -> usize {
    30
}

fn default_redis_url() -> String {
    "redis://127.0.0.1:6379".to_string()
}

fn default_similarity_cutoff() -> f64 {
    0.85
}

fn default_ttl() -> u64 {
    3600
}

fn default_input_cost() -> f64 {
    0.003
}

impl Default for ServerConfig {
    fn default() -> Self {
        default_server()
    }
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            tool_dedup: true,
            regex_structural: true,
            ast_pruning: true,
            min_lines_for_ast: 30,
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            redis_url: default_redis_url(),
            similarity_cutoff: 0.85,
            ttl_seconds: 3600,
        }
    }
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            input_cost_per_1k: 0.003,
        }
    }
}

impl JanusConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            let config: JanusConfig = toml::from_str(&content)?;
            Ok(config)
        } else {
            tracing::warn!("Config file not found at {}, using defaults", path.display());
            Ok(Self {
                server: ServerConfig::default(),
                pipeline: PipelineConfig::default(),
                cache: CacheConfig::default(),
                pricing: PricingConfig::default(),
            })
        }
    }
}
