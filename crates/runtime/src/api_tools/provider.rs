//! ApiToolProvider — implements ToolProvider for declarative API tools.
//!
//! Reads an ApiToolDef, generates a PluginToolDef, and on execute():
//! resolves params → builds URL → reqwest call → extracts JSON fields → returns result.

use types::api_tool::ApiToolDef;
use types::plugin::PluginToolContext;
use types::tool::PluginToolDef;
use types::tool::{PluginToolError, ToolProvider};
use serde_json::Value;

pub struct ApiToolProvider {
    config: ApiToolDef,
    http: reqwest::Client,
}

impl ApiToolProvider {
    pub fn new(config: ApiToolDef) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { config, http }
    }

    /// Read API key from env var if configured.
    fn resolve_auth(&self) -> Option<String> {
        if let Some(ref env_name) = self.config.auth_env {
            std::env::var(env_name).ok().filter(|s| !s.is_empty())
        } else {
            None
        }
    }

    /// Build the full URL with query params from input args.
    fn build_url(&self, args: &Value) -> String {
        let mut url = self.config.url.clone();

        // Replace {param_name} placeholders in URL template
        for (name, _param_def) in &self.config.params {
            if let Some(val) = args.get(name).and_then(|v| v.as_str()) {
                let placeholder = format!("{{{}}}", name);
                url = url.replace(&placeholder, &urlencoding::encode(val));
            }
        }

        // Build query string from remaining params not already in URL
        let mut query_parts: Vec<String> = Vec::new();

        for (name, _param_def) in &self.config.params {
            // Skip params already embedded in URL as {param} placeholders
            if self.config.url.contains(&format!("{{{}}}", name)) {
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
            }
        }

        // Add default values for params not provided by the agent
        for (name, param_def) in &self.config.params {
            if args.get(name).is_none() {
                if let Some(ref default) = param_def.default {
                    let val_str = match default {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => n.to_string(),
                        Value::Bool(b) => b.to_string(),
                        _ => continue,
                    };
                    query_parts.push(format!("{}={}", urlencoding::encode(name), urlencoding::encode(&val_str)));
                }
            }
        }

        // Append auth param
        if let (Some(auth_key), Some(auth_param)) = (self.resolve_auth(), &self.config.auth_param) {
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

    /// Navigate a dot-path into a JSON value.
    /// Supports "field.subfield[0].name" notation.
    fn navigate_path<'a>(&self, root: &'a Value, path: &str) -> Option<&'a Value> {
        let mut current = root;
        for segment in path.split('.') {
            if segment.is_empty() {
                continue;
            }
            // Check for array index: "field[0]"
            if let Some(bracket) = segment.find('[') {
                let field = &segment[..bracket];
                let index_str = &segment[bracket + 1..segment.len() - 1]; // strip [ and ]
                if !field.is_empty() {
                    current = current.get(field)?;
                }
                let index: usize = index_str.parse().ok()?;
                current = current.get(index)?;
            } else {
                current = current.get(segment)?;
            }
        }
        Some(current)
    }

    /// Apply a built-in transform to a numeric value.
    fn apply_transform(&self, value: f64, transform: &str) -> Value {
        match transform {
            "divide_1000_round1" => {
                let result = (value / 1000.0 * 10.0).round() / 10.0;
                Value::from(serde_json::Number::from_f64(result).unwrap_or_else(|| serde_json::Number::from(0)))
            }
            "divide_60_round" => {
                let result = (value / 60.0).round() as i64;
                Value::from(result)
            }
            "to_int" => Value::from(value as i64),
            "to_float" => {
                Value::from(serde_json::Number::from_f64(value).unwrap_or_else(|| serde_json::Number::from(0)))
            }
            "round1" => {
                let result = (value * 10.0).round() / 10.0;
                Value::from(serde_json::Number::from_f64(result).unwrap_or_else(|| serde_json::Number::from(0)))
            }
            "round0" => Value::from(value.round() as i64),
            _ => Value::from(value as i64),
        }
    }

    /// Extract a value from the response according to extract config.
    fn extract_value(&self, response: &Value, extract_def: &types::api_tool::ApiExtractDef, extracted: &serde_json::Map<String, Value>) -> Option<Value> {
        // Derived field: compute from already-extracted fields
        if extract_def.derived.unwrap_or(false) {
            if let Some(ref tiers) = extract_def.tiers {
                if let Some(ref from) = extract_def.from {
                    if let Some(from_val) = extracted.get(from) {
                        let num = from_val.as_f64().unwrap_or(0.0);
                        for tier in tiers {
                            if let Some(le) = tier.le {
                                if num <= le {
                                    return Some(Value::String(tier.value.clone()));
                                }
                            }
                        }
                        // Default tier (no le field) — last one
                        if let Some(last) = tiers.last() {
                            if last.le.is_none() {
                                return Some(Value::String(last.value.clone()));
                            }
                        }
                    }
                }
            }
            return None;
        }

        // Normal extraction from response
        let path = extract_def.path.as_deref()?;
        let raw = self.navigate_path(response, path)?;

        // Get numeric value
        let num = match raw {
            Value::Number(n) => n.as_f64().unwrap_or(0.0),
            Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
            _ => return Some(raw.clone()),
        };

        // Apply transform or type cast
        if let Some(ref transform) = extract_def.transform {
            Some(self.apply_transform(num, transform))
        } else if let Some(ref t) = extract_def.r#type {
            match t.as_str() {
                "int" => Some(Value::from(num as i64)),
                "float" => Some(Value::from(serde_json::Number::from_f64(num).unwrap_or_else(|| serde_json::Number::from(0)))),
                _ => Some(raw.clone()),
            }
        } else {
            Some(raw.clone())
        }
    }

    /// Check error condition in response.
    fn check_error(&self, response: &Value) -> Result<(), String> {
        if let Some(ref check) = self.config.error_check {
            let actual = self.navigate_path(response, &check.field)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if actual != check.expect {
                return Err(format!("API error: {}={}', expected='{}", check.field, actual, check.expect));
            }
        }
        Ok(())
    }
}

impl ToolProvider for ApiToolProvider {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: self.config.name.clone(),
            description: self.config.description.clone(),
            parameters_json: self.config.input_schema_json(),
        }
    }

    fn execute(
        &self,
        args: &Value,
        _context: &PluginToolContext,
    ) -> Result<String, PluginToolError> {
        // Validate required params
        for (name, param_def) in &self.config.params {
            if param_def.required && args.get(name).is_none() && param_def.default.is_none() {
                return Err(PluginToolError::tool(format!("Missing required parameter: {}", name)));
            }
        }

        let url = self.build_url(args);

        // Build reqwest request
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| PluginToolError::tool(format!("Runtime error: {e}")))?;

        let method = self.config.method.to_uppercase();
        let config_name = self.config.name.clone();
        let config_headers = self.config.headers.clone();

        rt.block_on(async {
            let mut req = match method.as_str() {
                "POST" => self.http.post(&url),
                "PUT" => self.http.put(&url),
                "PATCH" => self.http.patch(&url),
                "DELETE" => self.http.delete(&url),
                _ => self.http.get(&url),
            };

            for (k, v) in &config_headers {
                req = req.header(k.as_str(), v.as_str());
            }

            let resp = req
                .send()
                .await
                .map_err(|e| PluginToolError::tool(format!("{} request failed: {e}", config_name)))?;

            let status = resp.status();
            if !status.is_success() {
                return Err(PluginToolError::tool(format!("{} HTTP error: {}", config_name, status)));
            }

            let body: Value = resp
                .json()
                .await
                .map_err(|e| PluginToolError::tool(format!("{} parse error: {e}", config_name)))?;

            // Check error condition
            if let Err(e) = self.check_error(&body) {
                return Err(PluginToolError::tool(e));
            }

            // If no extract rules, return raw response
            if self.config.extract.is_empty() {
                return Ok(serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()));
            }

            // Extract fields
            let mut extracted = serde_json::Map::new();

            // First pass: extract non-derived fields
            for (name, def) in &self.config.extract {
                if !def.derived.unwrap_or(false) {
                    if let Some(val) = self.extract_value(&body, def, &extracted) {
                        extracted.insert(name.clone(), val);
                    }
                }
            }

            // Second pass: extract derived fields (they depend on first-pass results)
            for (name, def) in &self.config.extract {
                if def.derived.unwrap_or(false) {
                    if let Some(val) = self.extract_value(&body, def, &extracted) {
                        extracted.insert(name.clone(), val);
                    }
                }
            }

            let result = Value::Object(extracted);
            Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
        })
    }
}
