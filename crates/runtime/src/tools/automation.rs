//! Automation-rule tools (admin-gated): list/upsert/delete per-app
//! "trigger -> fixed action" rules stored in `automation_rules`. The channel
//! layer matches these on inbound events (subscribe/keyword) and delivers the
//! fixed reply WITHOUT routing to the agent LLM.
//!
//! Admin gate uses the 86bus `wechat_identity` role (`"admin"`), distinct from
//! the clone-admin (`is_admin_gated`) concept - do not mix them.

use std::sync::Arc;

use super::ToolModule;
use crate::kernel_handle::KernelHandle;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::automation::{AutomationRule, TaskKind, TriggerKind};
use types::error::{CarrierError, CarrierResult};
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;

pub struct AutomationRulesTools;

/// 86bus admin gate. Only callers whose `wechat_identity` role is `"admin"`
/// may manage automation rules.
fn require_admin(sender_id: Option<&str>) -> CarrierResult<()> {
    let sid = sender_id.ok_or_else(|| {
        CarrierError::CapabilityDenied("automation_rule: no sender_id in context".into())
    })?;
    if crate::wechat_identity::get(sid).as_deref() != Some("admin") {
        return Err(CarrierError::CapabilityDenied(
            "automation_rule tools require 86bus admin role".into(),
        ));
    }
    Ok(())
}

#[async_trait]
impl ToolModule for AutomationRulesTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "automation_rule_list".to_string(),
                description: "List automation rules for a weixin-oa app_id (admin only). Rules fire fixed replies on subscribe/keyword events without invoking the agent.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "app_id": { "type": "string", "description": "Service-account app_id (bot_id)" },
                        "channel": { "type": "string", "description": "Channel name (default 'weixin-oa')" }
                    },
                    "required": ["app_id"]
                }),
            },
            ToolDefinition {
                name: "automation_rule_upsert".to_string(),
                description: "Create or update an automation rule (admin only). On a matching inbound event, deliver a fixed reply without the agent LLM. trigger: subscribe|keyword; task: push_text|push_miniprogram.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "app_id": { "type": "string" },
                        "name": { "type": "string", "description": "Human-readable rule name" },
                        "trigger": { "type": "string", "enum": ["subscribe", "keyword"] },
                        "keyword": { "type": "string", "description": "Required when trigger=keyword (substring match on text)" },
                        "task": { "type": "string", "enum": ["push_text", "push_miniprogram"] },
                        "text": { "type": "string", "description": "Required when task=push_text" },
                        "miniprogram": { "type": "object", "description": "Required when task=push_miniprogram: {appid, pagepath, title, thumb_media_id}" },
                        "priority": { "type": "integer", "description": "Higher = evaluated first (default 0)" },
                        "enabled": { "type": "boolean", "description": "default true" },
                        "id": { "type": "string", "description": "Existing rule id to update (omit to create new)" }
                    },
                    "required": ["app_id", "name", "trigger", "task"]
                }),
            },
            ToolDefinition {
                name: "automation_rule_delete".to_string(),
                description: "Delete an automation rule by id (admin only).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" } },
                    "required": ["id"]
                }),
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<CarrierResult<String>> {
        let kernel = ctx.kernel;
        let sender_id = ctx.sender_id;
        match name {
            "automation_rule_list" => Some(tool_rule_list(input, kernel, sender_id).await),
            "automation_rule_upsert" => Some(tool_rule_upsert(input, kernel, sender_id).await),
            "automation_rule_delete" => Some(tool_rule_delete(input, kernel, sender_id).await),
            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> PermissionLevel {
        match tool_name {
            "automation_rule_list" | "automation_rule_upsert" | "automation_rule_delete" => {
                PermissionLevel::Write
            }
            _ => PermissionLevel::Dangerous,
        }
    }
}

async fn tool_rule_list(
    input: &Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    sender_id: Option<&str>,
) -> CarrierResult<String> {
    require_admin(sender_id)?;
    let kh = crate::tools::require_kernel(kernel)?;
    let app_id = input["app_id"]
        .as_str()
        .ok_or_else(|| CarrierError::InvalidInput("Missing 'app_id'".to_string()))?;
    let channel = input["channel"].as_str().unwrap_or("weixin-oa").to_string();
    let rules = kh.automation_rule_list(&channel, app_id).await?;
    serde_json::to_string_pretty(&rules).map_err(|e| CarrierError::Serialization(e.to_string()))
}

async fn tool_rule_upsert(
    input: &Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    sender_id: Option<&str>,
) -> CarrierResult<String> {
    require_admin(sender_id)?;
    let kh = crate::tools::require_kernel(kernel)?;

    let app_id = input["app_id"]
        .as_str()
        .ok_or_else(|| CarrierError::InvalidInput("Missing 'app_id'".to_string()))?
        .to_string();
    let name = input["name"]
        .as_str()
        .ok_or_else(|| CarrierError::InvalidInput("Missing 'name'".to_string()))?
        .to_string();
    let channel = input["channel"].as_str().unwrap_or("weixin-oa").to_string();
    let trigger = input["trigger"]
        .as_str()
        .ok_or_else(|| CarrierError::InvalidInput("Missing 'trigger'".to_string()))?;
    let task = input["task"]
        .as_str()
        .ok_or_else(|| CarrierError::InvalidInput("Missing 'task'".to_string()))?;

    let trigger_kind = match trigger {
        "subscribe" => TriggerKind::Subscribe,
        "keyword" => TriggerKind::Keyword,
        other => {
            return Err(CarrierError::InvalidInput(format!(
                "unknown trigger '{other}' (subscribe|keyword)"
            )))
        }
    };
    let task_kind = match task {
        "push_text" => TaskKind::PushText,
        "push_miniprogram" => TaskKind::PushMiniprogram,
        other => {
            return Err(CarrierError::InvalidInput(format!(
                "unknown task '{other}' (push_text|push_miniprogram)"
            )))
        }
    };

    let trigger_data = match trigger_kind {
        TriggerKind::Keyword => input["keyword"]
            .as_str()
            .ok_or_else(|| {
                CarrierError::InvalidInput("trigger=keyword requires 'keyword'".to_string())
            })?
            .to_string(),
        TriggerKind::Subscribe => String::new(),
    };

    let task_payload = match task_kind {
        TaskKind::PushText => {
            let text = input["text"].as_str().ok_or_else(|| {
                CarrierError::InvalidInput("task=push_text requires 'text'".to_string())
            })?;
            serde_json::json!({ "text": text })
        }
        TaskKind::PushMiniprogram => {
            let mp = input.get("miniprogram").ok_or_else(|| {
                CarrierError::InvalidInput(
                    "task=push_miniprogram requires 'miniprogram' {appid,pagepath,title,thumb_media_id}"
                        .to_string(),
                )
            })?;
            let appid = mp["appid"].as_str().ok_or_else(|| {
                CarrierError::InvalidInput("miniprogram.appid required".to_string())
            })?;
            let pagepath = mp["pagepath"].as_str().ok_or_else(|| {
                CarrierError::InvalidInput("miniprogram.pagepath required".to_string())
            })?;
            let title = mp["title"].as_str().ok_or_else(|| {
                CarrierError::InvalidInput("miniprogram.title required".to_string())
            })?;
            let thumb_media_id = mp["thumb_media_id"].as_str().ok_or_else(|| {
                CarrierError::InvalidInput("miniprogram.thumb_media_id required".to_string())
            })?;
            serde_json::json!({
                "miniprogram": {
                    "appid": appid,
                    "pagepath": pagepath,
                    "title": title,
                    "thumb_media_id": thumb_media_id
                }
            })
        }
    };

    let priority = input["priority"].as_i64().unwrap_or(0);
    let enabled = input["enabled"].as_bool().unwrap_or(true);
    let id = input["id"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let now = chrono::Utc::now().to_rfc3339();

    let rule = AutomationRule {
        id: id.clone(),
        app_id,
        channel,
        name,
        enabled,
        priority,
        trigger_kind,
        trigger_data,
        task_kind,
        task_payload,
        created_at: now.clone(),
        updated_at: now,
    };
    kh.automation_rule_upsert(rule).await?;
    Ok(format!("Automation rule saved: {id}"))
}

async fn tool_rule_delete(
    input: &Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    sender_id: Option<&str>,
) -> CarrierResult<String> {
    require_admin(sender_id)?;
    let kh = crate::tools::require_kernel(kernel)?;
    let id = input["id"]
        .as_str()
        .ok_or_else(|| CarrierError::InvalidInput("Missing 'id'".to_string()))?;
    kh.automation_rule_delete(id).await?;
    Ok(format!("Automation rule deleted: {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_admin_gate() {
        // 86bus role gate: only role=="admin" passes. Distinct from clone-admin.
        crate::wechat_identity::set("sid_nonadmin", "carrier_user");
        assert!(require_admin(Some("sid_nonadmin")).is_err());
        crate::wechat_identity::set("sid_admin", "admin");
        assert!(require_admin(Some("sid_admin")).is_ok());
        assert!(require_admin(None).is_err()); // no sender_id
        crate::wechat_identity::set("sid_empty", "");
        assert!(require_admin(Some("sid_empty")).is_err()); // empty role != admin
    }
}
