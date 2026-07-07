//! DeclarativeApiModule — implements ToolModule for all api_tools.toml tools.
//!
//! Loaded at startup from api_tools.toml. Each tool definition becomes a
//! ToolDefinition that agents can see directly. On execute(), the matching
//! config is found, reqwest fires the HTTP call, and extracted fields are
//! returned to the agent.

use crate::tools::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::api_tool::ApiToolDef;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;

pub struct DeclarativeApiModule {
    tools: Vec<ApiToolDef>,
    http: reqwest::Client,
}

impl DeclarativeApiModule {
    pub fn new(tools: Vec<ApiToolDef>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { tools, http }
    }

    fn find_config(&self, name: &str) -> Option<&ApiToolDef> {
        self.tools.iter().find(|t| t.name == name)
    }

    fn resolve_auth(config: &ApiToolDef) -> Option<String> {
        if let Some(ref env_name) = config.auth_env {
            std::env::var(env_name).ok().filter(|s| !s.is_empty())
        } else {
            None
        }
    }

    fn build_url(config: &ApiToolDef, args: &Value) -> String {
        let mut url = config.url.clone();

        // Replace {param_name} placeholders in URL template
        for name in config.params.keys() {
            if let Some(val) = args.get(name).and_then(|v| v.as_str()) {
                let placeholder = format!("{{{}}}", name);
                url = url.replace(&placeholder, &urlencoding::encode(val));
            }
        }

        // Build query string for params not already embedded as {param} in URL
        let mut query_parts: Vec<String> = Vec::new();

        for (name, param_def) in &config.params {
            if config.url.contains(&format!("{{{}}}", name)) {
                continue;
            }
            if let Some(val) = args.get(name) {
                let val_str = match val {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => continue,
                };
                query_parts.push(format!("{}={}", urlencoding::encode(name), urlencoding::encode(&val_str)));
            } else if let Some(ref default) = param_def.default {
                let val_str = match default {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => continue,
                };
                query_parts.push(format!("{}={}", urlencoding::encode(name), urlencoding::encode(&val_str)));
            }
        }

        // Append auth param
        if let (Some(auth_key), Some(auth_param)) = (Self::resolve_auth(config), &config.auth_param) {
            query_parts.push(format!("{}={}", urlencoding::encode(auth_param), urlencoding::encode(&auth_key)));
        }

        if query_parts.is_empty() {
            url
        } else if url.contains('?') {
            format!("{}&{}", url, query_parts.join("&"))
        } else {
            format!("{}?{}", url, query_parts.join("&"))
        }
    }

    /// Navigate a dot-path into a JSON value: "route.paths[0].distance"
    fn navigate_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
        let mut current = root;
        for segment in path.split('.') {
            if segment.is_empty() {
                continue;
            }
            if let Some(bracket) = segment.find('[') {
                let field = &segment[..bracket];
                let idx_str = &segment[bracket + 1..segment.len() - 1];
                if !field.is_empty() {
                    current = current.get(field)?;
                }
                let idx: usize = idx_str.parse().ok()?;
                current = current.get(idx)?;
            } else {
                current = current.get(segment)?;
            }
        }
        Some(current)
    }

    fn apply_transform(value: f64, transform: &str) -> Value {
        match transform {
            "divide_1000_round1" => {
                let r = (value / 1000.0 * 10.0).round() / 10.0;
                Value::from(serde_json::Number::from_f64(r).unwrap_or_else(|| serde_json::Number::from(0)))
            }
            "divide_60_round" => Value::from((value / 60.0).round() as i64),
            "to_int" => Value::from(value as i64),
            "round1" => {
                let r = (value * 10.0).round() / 10.0;
                Value::from(serde_json::Number::from_f64(r).unwrap_or_else(|| serde_json::Number::from(0)))
            }
            "round0" => Value::from(value.round() as i64),
            _ => Value::from(value as i64),
        }
    }

    /// Execute a single API tool call.
    /// Resolve parameters that have a [tool.resolve] config.
    /// For each param with a resolve rule, if the condition is met, call the
    /// specified tool to transform the value (e.g. geocode place name → coordinates).
    async fn resolve_params(&self, config: &ApiToolDef, args: &Value) -> Result<Value, String> {
        if config.resolve.is_empty() {
            return Ok(args.clone());
        }

        let mut resolved = args.clone();

        for (param_name, resolve_def) in &config.resolve {
            // Only resolve if the param exists in args
            let current_val = match resolved.get(param_name).and_then(|v| v.as_str()) {
                Some(v) => v.to_string(),
                None => continue,
            };

            // Check condition
            let condition = resolve_def.condition.as_deref().unwrap_or("");
            let should_resolve = match condition {
                "not_coordinates" => !is_coordinates(&current_val),
                "not_empty" => !current_val.is_empty(),
                "" => true, // no condition = always resolve
                _ => true,
            };

            if !should_resolve {
                continue;
            }

            // Find the resolve target tool config
            let target_config = match self.find_config(&resolve_def.tool) {
                Some(c) => c,
                None => {
                    tracing::warn!(
                        param = %param_name,
                        tool = %resolve_def.tool,
                        "resolve: target tool not found, skipping"
                    );
                    continue;
                }
            };

            // Call the target tool with the specified param
            let mut resolve_args = serde_json::Map::new();
            resolve_args.insert(resolve_def.param.clone(), Value::String(current_val));

            tracing::info!(
                param = %param_name,
                tool = %resolve_def.tool,
                "resolve: pre-resolving parameter"
            );

            match Box::pin(self.execute_api_call(target_config, &Value::Object(resolve_args))).await {
                Ok(result_str) => {
                    // Extract the specified field from the result
                    let result: Value = serde_json::from_str(&result_str)
                        .unwrap_or(Value::Null);
                    if let Some(extracted) = result.get(&resolve_def.extract) {
                        if let Some(s) = extracted.as_str() {
                            resolved[param_name] = Value::String(s.to_string());
                            tracing::info!(
                                param = %param_name,
                                resolved = %s,
                                "resolve: parameter resolved"
                            );
                        } else {
                            tracing::warn!(param = %param_name, "resolve: extracted value is not a string");
                        }
                    } else {
                        tracing::warn!(
                            param = %param_name,
                            field = %resolve_def.extract,
                            "resolve: field not found in result"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(param = %param_name, error = %e, "resolve: failed, using original value");
                }
            }
        }

        Ok(resolved)
    }

    async fn execute_api_call(&self, config: &ApiToolDef, args: &Value) -> Result<String, String> {
        // Validate required params
        for (name, param_def) in &config.params {
            if param_def.required && args.get(name).is_none() && param_def.default.is_none() {
                return Err(format!("Missing required parameter: {}", name));
            }
        }

        // Resolve params: if config.resolve has entries, pre-process args
        let resolved_args = self.resolve_params(config, args).await?;

        let url = Self::build_url(config, &resolved_args);
        let method = config.method.to_uppercase();

        let mut req = match method.as_str() {
            "POST" => self.http.post(&url),
            "PUT" => self.http.put(&url),
            "PATCH" => self.http.patch(&url),
            "DELETE" => self.http.delete(&url),
            _ => self.http.get(&url),
        };

        for (k, v) in &config.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let resp = req.send().await.map_err(|e| format!("{} request failed: {}", config.name, e))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("{} HTTP error: {}", config.name, status));
        }

        let body: Value = resp.json().await.map_err(|e| format!("{} parse error: {}", config.name, e))?;

        // Error check
        if let Some(ref check) = config.error_check {
            let actual = Self::navigate_path(&body, &check.field)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if actual != check.expect {
                return Err(format!("{} API error: {}='{}', expected='{}'", config.name, check.field, actual, check.expect));
            }
        }

        // No extract rules → return raw response
        if config.extract.is_empty() {
            return Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()));
        }

        // Extract fields — two passes (non-derived first, then derived)
        let mut extracted = serde_json::Map::new();

        for (name, def) in &config.extract {
            if def.derived.unwrap_or(false) {
                continue;
            }
            if let Some(ref path) = def.path {
                if let Some(raw) = Self::navigate_path(&body, path) {
                    let num = match raw {
                        Value::Number(n) => n.as_f64().unwrap_or(0.0),
                        Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
                        _ => {
                            extracted.insert(name.clone(), raw.clone());
                            continue;
                        }
                    };
                    if let Some(ref transform) = def.transform {
                        extracted.insert(name.clone(), Self::apply_transform(num, transform));
                    } else if let Some(ref t) = def.r#type {
                        match t.as_str() {
                            "int" => { extracted.insert(name.clone(), Value::from(num as i64)); }
                            "float" => {
                                let n = serde_json::Number::from_f64(num).unwrap_or_else(|| serde_json::Number::from(0));
                                extracted.insert(name.clone(), Value::from(n));
                            }
                            _ => { extracted.insert(name.clone(), raw.clone()); }
                        }
                    } else {
                        extracted.insert(name.clone(), raw.clone());
                    }
                }
            }
        }

        // Derived fields (tier mapping)
        for (name, def) in &config.extract {
            if !def.derived.unwrap_or(false) {
                continue;
            }
            if let Some(ref tiers) = def.tiers {
                if let Some(ref from) = def.from {
                    if let Some(from_val) = extracted.get(from) {
                        let num = from_val.as_f64().unwrap_or(0.0);
                        for tier in tiers {
                            if let Some(le) = tier.le {
                                if num <= le {
                                    extracted.insert(name.clone(), Value::String(tier.value.clone()));
                                    break;
                                }
                            } else {
                                // Default tier (no le) — last entry
                                extracted.insert(name.clone(), Value::String(tier.value.clone()));
                            }
                        }
                    }
                }
            }
        }

        let result = Value::Object(extracted);
        Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
    }
}

#[async_trait]
impl ToolModule for DeclarativeApiModule {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| ToolDefinition {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: serde_json::from_str(&t.input_schema_json()).unwrap_or(Value::Object(serde_json::Map::new())),
        }).collect()
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        let config = self.find_config(name)?;
        let result = self.execute_api_call(config, input).await;
        Some(result)
    }

    fn permission_level(&self, _tool_name: &str) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }
}


/// Check if a string looks like coordinates (contains comma, no CJK chars).
fn is_coordinates(s: &str) -> bool {
    s.contains(',') && !s.chars().any(|c| c > '\u{4e00}' && c < '\u{9fff}')
}
