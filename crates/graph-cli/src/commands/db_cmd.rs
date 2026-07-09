//! `graph db` — raw access to the embedded database.

use crate::cli::DbCommand;
use crate::runtime::open_ladybug;
use anyhow::Result;

pub async fn run(command: DbCommand) -> Result<()> {
    let config = graph_config::load()?.config;
    let store = open_ladybug(&config)?;
    match command {
        DbCommand::Query { cypher, json } => {
            let rows = store.raw_query(&cypher).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for row in rows {
                    println!("{}", row.join("\t"));
                }
            }
            Ok(())
        }
    }
}
