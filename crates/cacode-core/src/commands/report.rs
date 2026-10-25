use anyhow::Result;
use rusqlite::Connection;
use serde_json::json;
use dirs;


pub fn run(format: String) -> Result<()> {
    // Open DB
    let db_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Home directory not found"))?
        .join(".cacode")
        .join("usage.db");
    if !db_path.exists() {
        println!("[report] No usage data found at {}", db_path.display());
        return Ok(());
    }
    let conn = Connection::open(&db_path)?;
    // Aggregate per model
    let mut stmt = conn.prepare(
        "SELECT model, COUNT(*) as calls, SUM(prompt_tokens) as prompt, SUM(completion_tokens) as completion, SUM(total_tokens) as total, SUM(est_cost) as cost FROM token_usage GROUP BY model",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, f64>(5)?,
        ))
    })?;
    let mut data = Vec::new();
    for r in rows {
        data.push(r?);
    }
    match format.as_str() {
        "json" => {
            let json_output = json!(data.iter().map(|(model, calls, prompt, completion, total, cost)| {
                let mut map = serde_json::Map::new();
                map.insert("model".to_string(), json!(model));
                map.insert("calls".to_string(), json!(calls));
                map.insert("prompt_tokens".to_string(), json!(prompt));
                map.insert("completion_tokens".to_string(), json!(completion));
                map.insert("total_tokens".to_string(), json!(total));
                map.insert("estimated_cost".to_string(), json!(cost));
                json!(map)
            }).collect::<Vec<_>>());
            println!("{}", serde_json::to_string_pretty(&json_output)?);
        }
        "csv" => {
            // Header
            println!("model,calls,prompt_tokens,completion_tokens,total_tokens,estimated_cost");
            for (model, calls, prompt, completion, total, cost) in data {
                println!("{},{},{},{},{},{}", model, calls, prompt, completion, total, cost);
            }
        }
        _ => {
            // Table format
            println!("+----------------------+-------+--------------+-----------------+-------------+-----------------+");
            println!("| Model                | Calls | Prompt Tokens| Completion Tokens| Total Tokens| Estimated Cost |");
            println!("+----------------------+-------+--------------+-----------------+-------------+-----------------+");
            for (model, calls, prompt, completion, total, cost) in data {
                println!("| {:20} | {:5} | {:12} | {:15} | {:11} | {:15.6} |", model, calls, prompt, completion, total, cost);
            }
            println!("+----------------------+-------+--------------+-----------------+-------------+-----------------+");
        }
    }
    Ok(())
}
