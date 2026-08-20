//! DeclarativeApiModule — implements ToolModule for all api_tools.toml tools.
//!
//! Loaded at startup from api_tools.toml. Each tool definition becomes a
//! ToolDefinition that agents can see directly. On execute(), the matching
//! config is found, reqwest fires the HTTP call, and extracted fields are
//! returned to the agent.

use crate::tool_context::ToolContext;
use crate::tools::ToolModule;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use types::api_tool::ApiToolDef;
use types::error::{CarrierError, CarrierResult};
use types::tool::{PermissionLevel, ToolDefinition};

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
            // Use types::env::get_env (reads ENV_OVERRIDES from ~/.opencarrier/.env
            // first, then std::env) - std::env::var alone misses .env values.
            types::env::get_env(env_name).filter(|s| !s.is_empty())
        } else {
            None
        }
    }

    /// Param names that go into the JSON body (not the query string).
    fn body_field_set(config: &ApiToolDef) -> HashSet<String> {
        config
            .body
            .as_ref()
            .map(|b| b.fields.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Derive the URL path component (no scheme/host/query) for HMAC signing.
    /// e.g. "https://chuxing.86bus.com/api/ai/orders?x=1" -> "/api/ai/orders".
    fn url_path(url: &str) -> String {
        let after_scheme = match url.find("://") {
            Some(i) => &url[i + 3..],
            None => url,
        };
        let path_start = after_scheme
            .find('/')
            .map(|i| &after_scheme[i..])
            .unwrap_or("");
        match path_start.find('?') {
            Some(i) => path_start[..i].to_string(),
            None => path_start.to_string(),
        }
    }

    /// HMAC-SHA256(secret, msg) -> hex. Mirrors charter_sign in weixin-oa/tools.rs.
    fn hmac_sha256_hex(secret: &str, msg: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
        mac.update(msg.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Render the HMAC sign-string template. `{body}` is replaced LAST so body
    /// content can't be re-interpreted as another placeholder.
    fn render_sign_template(
        template: &str,
        method: &str,
        path: &str,
        timestamp: &str,
        key_id: &str,
        body: &str,
    ) -> String {
        template
            .replace("{method}", method)
            .replace("{path}", path)
            .replace("{timestamp}", timestamp)
            .replace("{key_id}", key_id)
            .replace("{body}", body)
    }

    /// Build the JSON request body from configured body.fields. Absent, null,
    /// or empty-string params are omitted (matches charter's skip_serializing_if
    /// for optional fields). Returns None when no body is configured.
    fn build_body_str(config: &ApiToolDef, args: &Value) -> Option<String> {
        let body_def = config.body.as_ref()?;
        let mut obj = serde_json::Map::new();
        for field in &body_def.fields {
            if let Some(val) = args.get(field) {
                if val.is_null() {
                    continue;
                }
                if let Value::String(s) = val {
                    if s.is_empty() {
                        continue;
                    }
                }
                obj.insert(field.clone(), val.clone());
            }
        }
        Some(serde_json::to_string(&Value::Object(obj)).unwrap_or_else(|_| "null".to_string()))
    }

    fn build_url(config: &ApiToolDef, args: &Value) -> String {
        let mut url = config.url.clone();
        let body_fields = Self::body_field_set(config);

        // Replace {param_name} placeholders in URL template
        for name in config.params.keys() {
            if let Some(val) = args.get(name).and_then(|v| v.as_str()) {
                let placeholder = format!("{{{}}}", name);
                url = url.replace(&placeholder, &urlencoding::encode(val));
            }
        }

        // Build query string for params not already embedded as {param} in URL,
        // and not destined for the JSON body.
        let mut query_parts: Vec<String> = Vec::new();

        for (name, param_def) in &config.params {
            if config.url.contains(&format!("{{{}}}", name)) {
                continue;
            }
            if body_fields.contains(name) {
                continue;
            }
            if let Some(val) = args.get(name) {
                let val_str = match val {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => continue,
                };
                query_parts.push(format!(
                    "{}={}",
                    urlencoding::encode(name),
                    urlencoding::encode(&val_str)
                ));
            } else if let Some(ref default) = param_def.default {
                let val_str = match default {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => continue,
                };
                query_parts.push(format!(
                    "{}={}",
                    urlencoding::encode(name),
                    urlencoding::encode(&val_str)
                ));
            }
        }

        // Append auth param
        if let (Some(auth_key), Some(auth_param)) = (Self::resolve_auth(config), &config.auth_param)
        {
            query_parts.push(format!(
                "{}={}",
                urlencoding::encode(auth_param),
                urlencoding::encode(&auth_key)
            ));
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
                Value::from(
                    serde_json::Number::from_f64(r).unwrap_or_else(|| serde_json::Number::from(0)),
                )
            }
            "divide_60_round" => Value::from((value / 60.0).round() as i64),
            "to_int" => Value::from(value as i64),
            "round1" => {
                let r = (value * 10.0).round() / 10.0;
                Value::from(
                    serde_json::Number::from_f64(r).unwrap_or_else(|| serde_json::Number::from(0)),
                )
            }
            "round0" => Value::from(value.round() as i64),
            _ => Value::from(value as i64),
        }
    }

    /// Execute a single API tool call.
    /// Resolve parameters that have a [tool.resolve] config.
    /// For each param with a resolve rule, if the condition is met, call the
    /// specified tool to transform the value (e.g. geocode place name → coordinates).
    async fn resolve_params(
        &self,
        config: &ApiToolDef,
        args: &Value,
        ctx: &ToolContext<'_>,
    ) -> CarrierResult<Value> {
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

            match Box::pin(self.execute_api_call(target_config, &Value::Object(resolve_args), ctx))
                .await
            {
                Ok(result_str) => {
                    // Extract the specified field from the result
                    let result: Value = serde_json::from_str(&result_str).unwrap_or(Value::Null);
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

    async fn execute_api_call(
        &self,
        config: &ApiToolDef,
        args: &Value,
        ctx: &ToolContext<'_>,
    ) -> CarrierResult<String> {
        // Validate required params
        for (name, param_def) in &config.params {
            if param_def.required && args.get(name).is_none() && param_def.default.is_none() {
                return Err(CarrierError::InvalidInput(format!(
                    "Missing required parameter: {}",
                    name
                )));
            }
        }

        // Resolve params: if config.resolve has entries, pre-process args
        let mut resolved_args = self.resolve_params(config, args, ctx).await?;

        // Inject context fields (e.g. sender_id -> openid, channel-gated). Only
        // fills fields the agent didn't already provide.
        if !config.inject.is_empty() {
            for (field, rule) in &config.inject {
                if resolved_args.get(field).is_some() {
                    continue;
                }
                // Skip if any guarded alternative is already provided (e.g.
                // don't inject openid when the agent gave phone/order_no).
                if rule
                    .only_if_absent
                    .iter()
                    .any(|f| resolved_args.get(f).is_some())
                {
                    continue;
                }
                if let Some(ref ch) = rule.channel {
                    if ctx.channel_type != Some(ch.as_str()) {
                        continue;
                    }
                }
                if rule.from == "sender_id" {
                    if let Some(sid) = ctx.sender_id {
                        if !sid.is_empty() {
                            resolved_args[field.clone()] = Value::String(sid.to_string());
                        }
                    }
                }
            }
        }

        // Build JSON body (if configured). The exact serialized string is both
        // signed (when hmac is set) and sent - never re-serialized.
        let body_str: Option<String> = Self::build_body_str(config, &resolved_args);

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

        // HMAC-SHA256 signing (e.g. 86bus /api/ai/ gateway).
        if let Some(ref hmac_def) = config.hmac {
            let key_id = types::env::get_env(&hmac_def.key_id_env).ok_or_else(|| {
                CarrierError::Internal(format!(
                    "{}: hmac.key_id_env '{}' not configured",
                    config.name, hmac_def.key_id_env
                ))
            })?;
            let secret = types::env::get_env(&hmac_def.secret_env).ok_or_else(|| {
                CarrierError::Internal(format!(
                    "{}: hmac.secret_env '{}' not configured",
                    config.name, hmac_def.secret_env
                ))
            })?;
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string();
            let path = Self::url_path(&config.url);
            let body_for_sign = body_str.as_deref().unwrap_or("");
            let sign_string = Self::render_sign_template(
                &hmac_def.sign_template,
                &method,
                &path,
                &timestamp,
                &key_id,
                body_for_sign,
            );
            let signature = Self::hmac_sha256_hex(&secret, &sign_string);
            for (k, v_template) in &hmac_def.headers {
                let v = v_template
                    .replace("{key_id}", &key_id)
                    .replace("{timestamp}", &timestamp)
                    .replace("{signature}", &signature);
                req = req.header(k.as_str(), v.as_str());
            }
        }

        // Attach body (sign-once/send-once: send the exact string we signed).
        if let Some(ref body_str) = body_str {
            req = req
                .header("Content-Type", "application/json")
                .body(body_str.clone());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| CarrierError::Network(format!("{} request failed: {}", config.name, e)))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(CarrierError::Network(format!(
                "{} HTTP error: {}",
                config.name, status
            )));
        }

        let body: Value = resp.json().await.map_err(|e| {
            CarrierError::Serialization(format!("{} parse error: {}", config.name, e))
        })?;

        // Error check. Render the field as a string whether it's a JSON string,
        // number, or bool (e.g. 86bus errcode is a number: 0 == "0").
        if let Some(ref check) = config.error_check {
            let actual = Self::navigate_path(&body, &check.field)
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => v.to_string(),
                })
                .unwrap_or_default();
            if actual != check.expect {
                return Err(CarrierError::Network(format!(
                    "{} API error: {}='{}', expected='{}'",
                    config.name, check.field, actual, check.expect
                )));
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
                            "int" => {
                                extracted.insert(name.clone(), Value::from(num as i64));
                            }
                            "float" => {
                                let n = serde_json::Number::from_f64(num)
                                    .unwrap_or_else(|| serde_json::Number::from(0));
                                extracted.insert(name.clone(), Value::from(n));
                            }
                            _ => {
                                extracted.insert(name.clone(), raw.clone());
                            }
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
                                    extracted
                                        .insert(name.clone(), Value::String(tier.value.clone()));
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
        self.tools
            .iter()
            .map(|t| ToolDefinition {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: serde_json::from_str(&t.input_schema_json())
                    .unwrap_or(Value::Object(serde_json::Map::new())),
            })
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<CarrierResult<String>> {
        let config = self.find_config(name)?;
        let result = self.execute_api_call(config, input, ctx).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    // Standard HMAC-SHA256 test vector (key="key", msg="The quick brown...").
    #[test]
    fn hmac_sha256_known_vector() {
        let sig = DeclarativeApiModule::hmac_sha256_hex(
            "key",
            "The quick brown fox jumps over the lazy dog",
        );
        assert_eq!(
            sig,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn url_path_strips_scheme_host_query() {
        assert_eq!(
            DeclarativeApiModule::url_path("https://chuxing.86bus.com/api/ai/orders"),
            "/api/ai/orders"
        );
        assert_eq!(
            DeclarativeApiModule::url_path("https://host.com/a/b?x=1&y=2"),
            "/a/b"
        );
        assert_eq!(DeclarativeApiModule::url_path("https://host.com/"), "/");
        assert_eq!(DeclarativeApiModule::url_path("https://host.com"), "");
    }

    #[test]
    fn render_sign_template_charter_shape() {
        let rendered = DeclarativeApiModule::render_sign_template(
            "{method}\n{path}\n{timestamp}\n{body}",
            "POST",
            "/api/ai/orders",
            "1700000000",
            "AK123",
            "{\"a\":1}",
        );
        assert_eq!(rendered, "POST\n/api/ai/orders\n1700000000\n{\"a\":1}");
    }

    /// Body containing a literal "{key_id}" must survive verbatim ({body} is
    /// substituted last, so body content is never re-interpreted).
    #[test]
    fn render_sign_template_body_literal_not_reinterpreted() {
        let rendered = DeclarativeApiModule::render_sign_template(
            "{method}\n{body}",
            "POST",
            "",
            "",
            "REAL_AK",
            "x{key_id}y",
        );
        assert_eq!(rendered, "POST\nx{key_id}y");
    }

    fn parse_tool(toml_str: &str) -> ApiToolDef {
        toml::from_str::<types::api_tool::ApiToolsConfig>(toml_str)
            .unwrap()
            .tool
            .into_iter()
            .next()
            .unwrap()
    }

    fn charter_test_config() -> ApiToolDef {
        parse_tool(
            r#"
[[tool]]
name = "charter_create_order"
description = "test"
url = "https://chuxing.86bus.com/api/ai/orders"
method = "POST"
[tool.body]
fields = ["username", "phone", "person_num", "start_point", "end_point", "go_time", "back_time", "remark"]
[tool.params]
username = { type = "string", description = "x" }
phone = { type = "string", description = "x" }
person_num = { type = "integer", description = "x" }
start_point = { type = "string", description = "x" }
end_point = { type = "string", description = "x" }
go_time = { type = "string", description = "x" }
back_time = { type = "string", description = "x" }
remark = { type = "string", description = "x" }
"#,
        )
    }

    #[test]
    fn build_body_str_omits_absent_and_empty() {
        let cfg = charter_test_config();
        let args = serde_json::json!({
            "username": "张三",
            "phone": "13800000000",
            "person_num": 5,
            "start_point": "南京南站",
            "end_point": "禄口机场",
            "go_time": "2026-08-11 08:00",
            "remark": ""
        });
        let body = DeclarativeApiModule::build_body_str(&cfg, &args).unwrap();
        assert!(!body.contains("back_time"));
        assert!(!body.contains("remark"));
        assert!(body.contains("username"));
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["username"], "张三");
        assert_eq!(v["person_num"], 5);
        assert!(v.get("back_time").is_none());
    }

    #[test]
    fn build_body_str_none_when_no_body_config() {
        let cfg = parse_tool(
            r#"
[[tool]]
name = "t"
description = "x"
url = "https://example.com/api"
method = "GET"
"#,
        );
        let args = serde_json::json!({"q": "hi"});
        assert!(DeclarativeApiModule::build_body_str(&cfg, &args).is_none());
    }

    /// Regression: the api_tool HMAC path must produce the SAME signature bytes
    /// as charter_sign (weixin-oa/tools.rs) for identical inputs, so migrating
    /// charter to a config-driven api_tool keeps the 86bus backend accepting it.
    #[test]
    fn charter_signature_matches_charter_sign_pattern() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let secret = "test-secret";
        let (method, path, timestamp) = ("POST", "/api/ai/orders", "1700000000");
        let body = r#"{"username":"张三","phone":"138","person_num":5,"start_point":"A","end_point":"B","go_time":"2026-08-11 08:00"}"#;

        // charter_sign equivalent (weixin-oa/tools.rs:250-260):
        let sign_str = format!("{method}\n{path}\n{timestamp}\n{body}");
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(sign_str.as_bytes());
        let expected_hex = hex::encode(mac.finalize().into_bytes());

        // api_tool path:
        let rendered = DeclarativeApiModule::render_sign_template(
            "{method}\n{path}\n{timestamp}\n{body}",
            method,
            path,
            timestamp,
            "",
            body,
        );
        let actual_hex = DeclarativeApiModule::hmac_sha256_hex(secret, &rendered);

        assert_eq!(sign_str, rendered);
        assert_eq!(actual_hex, expected_hex);
    }
}
