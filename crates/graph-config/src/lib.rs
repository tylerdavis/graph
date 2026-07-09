//! Layered TOML configuration for the graph CLI.
//!
//! Precedence (later wins): ~/.config/graph/config.toml < ./.graph/config.toml
//! < GRAPH_* environment variables < CLI flags.
