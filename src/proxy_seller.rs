//! Minimal Proxy-Seller resident-pool client used by the voice UI.

use serde::{Deserialize, Serialize};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://proxy-seller.com/personal/api/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxySellerOptions {
    pub list_name: String,
    pub ports: i64,
    pub rotation_seconds: i64,
    pub whitelist_ip: String,
}

impl Default for ProxySellerOptions {
    fn default() -> Self {
        Self {
            list_name: "nektome_app".into(),
            ports: 20,
            rotation_seconds: 3600,
            whitelist_ip: String::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProxySellerResult {
    pub proxies: Vec<String>,
    pub package_summary: String,
    pub list_id: i64,
}

pub struct ProxySellerClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    list_name: String,
    ports: i64,
    rotation_seconds: i64,
    whitelist_ip: String,
}

impl ProxySellerClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self, reqwest::Error> {
        Self::with_options(api_key, ProxySellerOptions::default())
    }

    pub fn with_options(
        api_key: impl Into<String>,
        options: ProxySellerOptions,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()?,
            api_key: api_key.into().trim().to_owned(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            list_name: options.list_name,
            ports: options.ports,
            rotation_seconds: options.rotation_seconds,
            whitelist_ip: options.whitelist_ip,
        })
    }

    pub async fn fetch_resident_proxies(&self) -> Result<ProxySellerResult, String> {
        if self.api_key.is_empty() {
            return Err("Proxy-Seller API key is not configured".into());
        }
        let base = format!("{}/{}/", self.base_url, self.api_key);
        let package = self.json(&format!("{base}resident/package")).await;
        let package_summary = match package {
            Ok(value) => match check_api_error(&value) {
                Ok(()) => package_summary(&value),
                Err(error) => format!("package unavailable: {error}"),
            },
            Err(error) => format!("package unavailable: {error}"),
        };

        let lists = self.json(&format!("{base}resident/lists")).await?;
        check_api_error(&lists)?;
        let mut list_id = find_list_id(&lists, &self.list_name).unwrap_or(0);
        if list_id == 0 {
            let payload = serde_json::json!({
                "title": self.list_name,
                "whitelist": self.whitelist_ip,
                "geo": {},
                "export": {"ports": self.ports, "ext": "txt"},
                "rotation": self.rotation_seconds,
            });
            let created = self
                .http
                .post(format!("{base}resident/list/add"))
                .json(&payload)
                .send()
                .await
                .map_err(|error| error.to_string())?
                .error_for_status()
                .map_err(|error| error.to_string())?
                .json::<Value>()
                .await
                .map_err(|error| error.to_string())?;
            check_api_error(&created)?;
            list_id = created
                .pointer("/data/id")
                .and_then(Value::as_i64)
                .ok_or("Proxy-Seller did not return the created list id")?;
        }

        let text = self
            .http
            .get(format!("{base}proxy/download/resident?listId={list_id}"))
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?
            .text()
            .await
            .map_err(|error| error.to_string())?;
        let mut proxies = text
            .lines()
            .map(str::trim)
            .filter(|line| line.contains('@'))
            .map(normalize_proxy_line)
            .collect::<Vec<_>>();
        proxies.sort_unstable_by_key(|value| value.to_ascii_lowercase());
        if proxies.is_empty() {
            return Err("Proxy-Seller returned an empty resident proxy list".into());
        }
        Ok(ProxySellerResult {
            proxies,
            package_summary,
            list_id,
        })
    }

    async fn json(&self, url: &str) -> Result<Value, String> {
        self.http
            .get(url)
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?
            .json()
            .await
            .map_err(|error| error.to_string())
    }
}

fn normalize_proxy_line(line: &str) -> String {
    let supported_scheme = line
        .split_once("://")
        .map(|(scheme, _)| {
            matches!(
                scheme.to_ascii_lowercase().as_str(),
                "http" | "https" | "socks5" | "socks5h"
            )
        })
        .unwrap_or(false);
    if supported_scheme {
        line.to_owned()
    } else {
        format!("http://{line}")
    }
}

fn check_api_error(value: &Value) -> Result<(), String> {
    if value
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status.eq_ignore_ascii_case("error"))
    {
        let message = value
            .get("errors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|error| error.get("message").and_then(Value::as_str))
            .collect::<Vec<_>>();
        let message = if message.is_empty() {
            "unknown error".to_owned()
        } else {
            message.join("; ")
        };
        if message.to_ascii_lowercase().contains("ip not allowed") {
            return Err(format!(
                "Proxy-Seller API: {message} (add the active IP to the Proxy-Seller whitelist or enter proxies manually)"
            ));
        }
        return Err(format!("Proxy-Seller API: {message}"));
    }
    Ok(())
}

fn find_list_id(value: &Value, preferred: &str) -> Option<i64> {
    let lists = value.get("data").and_then(Value::as_array)?;
    let fallback = lists
        .iter()
        .find_map(|item| item.get("id").and_then(Value::as_i64));
    lists
        .iter()
        .find(|item| {
            item.get("title")
                .and_then(Value::as_str)
                .is_some_and(|title| title.eq_ignore_ascii_case(preferred))
        })
        .and_then(|item| item.get("id").and_then(Value::as_i64))
        .or(fallback)
}

fn package_summary(value: &Value) -> String {
    let Some(data) = value.get("data") else {
        return "package unavailable".into();
    };
    let active = data
        .get("is_active")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let expired = data.get("expired_at").and_then(Value::as_str).unwrap_or("");
    let traffic = data
        .get("traffic_left")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
        .unwrap_or(0);
    let megabytes = traffic as f64 / (1024.0 * 1024.0);
    let traffic_text = if megabytes >= 1024.0 {
        format!("{:.2} GB", megabytes / 1024.0)
    } else {
        format!("{megabytes:.1} MB")
    };
    format!(
        "Resident package: {}, traffic: {traffic_text}, until: {expired}",
        if active { "active" } else { "inactive" }
    )
}

#[cfg(test)]
mod tests {
    use super::{check_api_error, find_list_id, normalize_proxy_line, package_summary};

    #[test]
    fn prefers_named_proxy_list_and_falls_back_to_first() {
        let lists =
            serde_json::json!({"data":[{"id":3,"title":"other"},{"id":7,"title":"NektoMe_App"}]});
        assert_eq!(find_list_id(&lists, "nektome_app"), Some(7));
        assert_eq!(find_list_id(&lists, "missing"), Some(3));
    }

    #[test]
    fn surfaces_structured_api_errors() {
        let error = serde_json::json!({"status":"error","errors":[{"message":"IP not allowed"}]});
        let message = check_api_error(&error).unwrap_err();
        assert!(message.starts_with("Proxy-Seller API: IP not allowed"));
        assert!(message.contains("whitelist"));
    }

    #[test]
    fn structured_api_error_status_is_case_insensitive() {
        let error = serde_json::json!({"status":"ERROR"});
        assert!(check_api_error(&error).is_err());
    }

    #[test]
    fn whitelist_errors_include_recovery_hint() {
        let error = serde_json::json!({
            "status":"error",
            "errors":[{"message":"IP not allowed 1.2.3.4"}]
        });
        let message = check_api_error(&error).unwrap_err();
        assert!(message.contains("whitelist"));
        assert!(message.contains("manually"));
    }

    #[test]
    fn formats_package_summary() {
        let package = serde_json::json!({"data":{"is_active":true,"traffic_left":1048576,"expired_at":"tomorrow"}});
        assert_eq!(
            package_summary(&package),
            "Resident package: active, traffic: 1.0 MB, until: tomorrow"
        );
    }

    #[test]
    fn preserves_proxy_schemes_case_insensitively() {
        assert_eq!(
            normalize_proxy_line("HTTP://u:p@host:1"),
            "HTTP://u:p@host:1"
        );
        assert_eq!(
            normalize_proxy_line("SOCKS5://u:p@host:1"),
            "SOCKS5://u:p@host:1"
        );
        assert_eq!(
            normalize_proxy_line("SOCKS5H://u:p@host:1"),
            "SOCKS5H://u:p@host:1"
        );
        assert_eq!(
            normalize_proxy_line("HTTPS://u:p@host:1"),
            "HTTPS://u:p@host:1"
        );
        assert_eq!(normalize_proxy_line("u:p@host:1"), "http://u:p@host:1");
    }
}
