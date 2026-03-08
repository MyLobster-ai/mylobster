//! Cron scheduling tool.

use super::{AgentTool, ToolContext, ToolInfo, ToolResult};
use anyhow::Result;
use async_trait::async_trait;

/// Schedule recurring jobs via cron expressions.
pub struct CronScheduleTool;

#[async_trait]
impl AgentTool for CronScheduleTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "cron_schedule".to_string(),
            description: "Schedule a recurring job using a cron expression".to_string(),
            category: "system".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "expression": {
                        "type": "string",
                        "description": "Cron expression (e.g. '0 9 * * 1-5' for weekdays at 9am)"
                    },
                    "message": {
                        "type": "string",
                        "description": "Message to process when the cron fires"
                    },
                    "name": {
                        "type": "string",
                        "description": "Human-readable name for this scheduled job"
                    },
                    "timezone": {
                        "type": "string",
                        "description": "IANA timezone (e.g. 'America/New_York'). Defaults to UTC."
                    }
                },
                "required": ["expression", "message"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let expression = params
            .get("expression")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing expression parameter"))?;

        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing message parameter"))?;

        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unnamed");

        let timezone = params
            .get("timezone")
            .and_then(|v| v.as_str());

        // Validate the cron expression (basic check)
        let parts: Vec<&str> = expression.split_whitespace().collect();
        if parts.len() < 5 || parts.len() > 6 {
            return Ok(ToolResult::error(format!(
                "Invalid cron expression '{}': expected 5 or 6 fields",
                expression
            )));
        }

        // Build the schedule
        let schedule = crate::cron::CronSchedule {
            kind: "cron".to_string(),
            expr: expression.to_string(),
            tz: timezone.map(|s| s.to_string()),
            stagger_ms: None,
        };

        // Compute stagger for logging
        let stagger = crate::cron::apply_stagger(&schedule, None);

        // Persist the job to the state directory
        let jobs_dir = context.config.state_dir.join("cron");
        let _ = tokio::fs::create_dir_all(&jobs_dir).await;

        let job_id = uuid::Uuid::new_v4().to_string();
        let job = serde_json::json!({
            "id": job_id,
            "name": name,
            "expression": expression,
            "message": message,
            "timezone": timezone,
            "session_key": context.session_key,
            "created_at": chrono::Utc::now().to_rfc3339(),
            "stagger_ms": stagger.as_millis() as u64
        });

        let job_path = jobs_dir.join(format!("{}.json", job_id));
        tokio::fs::write(&job_path, serde_json::to_string_pretty(&job)?).await?;

        tracing::info!(
            id = %job_id,
            name,
            expression,
            stagger_ms = stagger.as_millis() as u64,
            "cron job scheduled"
        );

        Ok(ToolResult::json(serde_json::json!({
            "scheduled": true,
            "id": job_id,
            "name": name,
            "expression": expression,
            "timezone": timezone,
            "stagger_ms": stagger.as_millis() as u64
        })))
    }
}

/// List all scheduled cron jobs.
pub struct CronListTool;

#[async_trait]
impl AgentTool for CronListTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "cron_list".to_string(),
            description: "List all scheduled cron jobs".to_string(),
            category: "system".to_string(),
            hidden: false,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult> {
        let jobs_dir = context.config.state_dir.join("cron");
        let mut jobs = Vec::new();

        if jobs_dir.exists() {
            let mut entries = tokio::fs::read_dir(&jobs_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    if let Ok(content) = tokio::fs::read_to_string(&path).await {
                        if let Ok(job) = serde_json::from_str::<serde_json::Value>(&content) {
                            jobs.push(job);
                        }
                    }
                }
            }
        }

        Ok(ToolResult::json(serde_json::json!({
            "jobs": jobs,
            "count": jobs.len()
        })))
    }
}
