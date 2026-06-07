//! MCP server bridging an agent to the running Imperium battle via BRP.
//!
//! Tools call the Bevy Remote Protocol (JSON-RPC at :15702) of the live game:
//! `battle_report` reads the force balance, `smite` removes a unit. Run the
//! game first (`cargo run -p imperium`), then point an MCP client at this.

use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router, transport::stdio, ServiceExt};
use serde_json::{json, Value};

const BRP: &str = "http://127.0.0.1:15702";

/// One JSON-RPC call to the game's BRP endpoint.
async fn brp(method: &str, params: Value) -> anyhow::Result<Value> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let resp = reqwest::Client::new()
        .post(BRP)
        .json(&body)
        .send()
        .await?
        .json::<Value>()
        .await?;
    Ok(resp)
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SmiteRequest {
    #[schemars(description = "team to strike: \"Red\" or \"Blue\"")]
    pub team: String,
}

#[derive(Debug, Clone)]
pub struct Imperium;

#[tool_router(server_handler)]
impl Imperium {
    #[tool(description = "Report the live battle: unit counts per team and per kind.")]
    async fn battle_report(&self) -> String {
        let params = json!({ "data": { "components": ["sim_core::Team", "sim_core::Kind"] } });
        match brp("world.query", params).await {
            Ok(v) => summarize(&v),
            Err(e) => format!("BRP error: {e} (is the game running?)"),
        }
    }

    #[tool(description = "Kill one unit of the given team (\"Red\" or \"Blue\") in the live battle.")]
    async fn smite(&self, Parameters(SmiteRequest { team }): Parameters<SmiteRequest>) -> String {
        let params = json!({ "data": { "components": ["sim_core::Team"] } });
        let v = match brp("world.query", params).await {
            Ok(v) => v,
            Err(e) => return format!("BRP error: {e} (is the game running?)"),
        };
        let target = v
            .get("result")
            .and_then(|r| r.as_array())
            .and_then(|arr| {
                arr.iter().find(|e| {
                    e.pointer("/components/sim_core::Team").and_then(Value::as_str) == Some(&team)
                })
            })
            .and_then(|e| e.get("entity").and_then(Value::as_u64));

        match target {
            Some(id) => match brp("world.despawn_entity", json!({ "entity": id })).await {
                Ok(_) => format!("Smote {team} entity {id}."),
                Err(e) => format!("despawn failed: {e}"),
            },
            None => format!("No {team} units found."),
        }
    }
}

fn summarize(v: &Value) -> String {
    let Some(arr) = v.get("result").and_then(|r| r.as_array()) else {
        return format!("unexpected BRP response: {v}");
    };
    let (mut red, mut blue) = (0, 0);
    let mut by: std::collections::BTreeMap<String, i32> = std::collections::BTreeMap::new();
    for e in arr {
        let team = e
            .pointer("/components/sim_core::Team")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let kind = e
            .pointer("/components/sim_core::Kind")
            .and_then(Value::as_str)
            .unwrap_or("?");
        match team {
            "Red" => red += 1,
            "Blue" => blue += 1,
            _ => {}
        }
        *by.entry(format!("  {team} {kind}")).or_insert(0) += 1;
    }
    let detail: Vec<String> = by.iter().map(|(k, n)| format!("{k}: {n}")).collect();
    format!("Red {red} vs Blue {blue}\n{}", detail.join("\n"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = Imperium.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
