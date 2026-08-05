// Registry payloads behind `GET /api/oracles` and `GET /api/config`.
//
// maw-ui-lite resolves the roster its summon panel offers through
// `fetchOracleRegistry` (`src/lib/oracleRegistry.ts`): it asks `/api/oracles`
// first and falls back to `/api/config` when the server predates the typed
// endpoint. maw-js only ever shipped the fallback, so the panel has always
// taken the config path. Serving neither left `knownAgents` empty and the
// panel unusable, which is what these two payloads fix.
//
// Both payloads read `config_load_layers()` only — the same merged
// `maw.config.*.json` layers the Config tab (`/api/config-file`) edits — and
// nothing else. They used to also merge in every `~/.maw/fleet/*.json` team
// registration's window names, so a team member auto-registered by `maw wake`
// stayed in the summon roster even after its fleet file went stale (repo
// deleted, session long gone): the roster had two sources of truth that could
// silently disagree. Team sub-agents aren't something a human picks from this
// picker anyway — they're spawned by their lead — so the fix is to drop the
// second source, not reconcile it.

/// Only `commands` entries naming an oracle join the roster; the map also holds
/// plain shell aliases.
const SERVEORACLES_ORACLE_SUFFIX: &str = "-oracle";

pub(crate) fn serveoracles_http_payload_read_only() -> Result<serde_json::Value, String> {
    let config = config_load_layers()?.config;
    Ok(serveoracles_payload_from_config(&config))
}

#[cfg(test)]
pub(crate) fn serveoracles_http_payload_from_config_file(
    path: &std::path::Path,
) -> Result<serde_json::Value, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("maw config: failed to read config: {error}"))?;
    let config = serde_json::from_str::<serde_json::Value>(&raw)
        .map_err(|error| format!("maw config: failed to parse config JSON: {error}"))?;
    Ok(serveoracles_payload_from_config(&config))
}

/// Masked config for `GET /api/config`.
pub(crate) fn serveconfig_http_payload_read_only() -> Result<serde_json::Value, String> {
    let loaded = config_load_layers()?;
    Ok(serveconfig_for_display(&loaded.config))
}

fn serveoracles_payload_from_config(config: &serde_json::Value) -> serde_json::Value {
    let oracles = serveoracles_names_from_config(config);
    serde_json::json!({
        "oracles": oracles,
        "count": oracles.len(),
        "version": MAW_RS_BUILD_VERSION,
    })
}

/// Mirrors `namesFromConfig` in maw-ui-lite's `oracleRegistry.ts`: every
/// `agents` and `sessions` key, plus `commands` keys that name an oracle.
///
/// Reading the same three maps the fallback reads is deliberate. The client
/// stops at the first 200 and never retries `/api/config`, so a typed endpoint
/// backed by a narrower source would silently hand back a smaller roster than
/// the fallback it replaces.
fn serveoracles_names_from_config(config: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();
    for key in ["agents", "sessions"] {
        serveoracles_extend_from_map(&mut names, config.get(key), false);
    }
    serveoracles_extend_from_map(&mut names, config.get("commands"), true);
    serveoracles_sorted_unique(names)
}

fn serveoracles_extend_from_map(
    names: &mut Vec<String>,
    value: Option<&serde_json::Value>,
    oracle_suffix_only: bool,
) {
    let Some(map) = value.and_then(serde_json::Value::as_object) else {
        return;
    };
    for key in map.keys() {
        if oracle_suffix_only && !key.ends_with(SERVEORACLES_ORACLE_SUFFIX) {
            continue;
        }
        if let Some(name) = serveoracles_canonical_name(key) {
            names.push(name);
        }
    }
}

/// Server-side twin of the client's `canonicalOracleName`.
///
/// The client canonicalises whatever it receives, so doing it here changes no
/// rendered row — it keeps the advertised `count` equal to the number of
/// entries the panel will actually show, instead of counting `atlas` and
/// `atlas-oracle` twice.
fn serveoracles_canonical_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let name = serveoracles_strip_oracle_suffix(trimmed);
    if name.is_empty() || name == "default" || name.starts_with('_') {
        return None;
    }
    Some(name.to_owned())
}

fn serveoracles_strip_oracle_suffix(value: &str) -> &str {
    let split_at = value.len().saturating_sub(SERVEORACLES_ORACLE_SUFFIX.len());
    if value.len() > SERVEORACLES_ORACLE_SUFFIX.len()
        && value[split_at..].eq_ignore_ascii_case(SERVEORACLES_ORACLE_SUFFIX)
    {
        &value[..split_at]
    } else {
        value
    }
}

/// Case-insensitive dedupe, then sort — the client's `sortedNames` contract.
///
/// `sortedNames` keeps whichever spelling it met first, which for the client is
/// the order keys appear in the config file. `serde_json` hands us the same keys
/// sorted, so first-seen here would be an unrelated pick — `ATLAS-ORACLE`
/// beating `atlas` purely because uppercase sorts earlier, and the panel would
/// render a shouting name. Prefer an already-lowercase spelling when the
/// variants disagree; the resulting set is identical either way.
fn serveoracles_sorted_unique(names: Vec<String>) -> Vec<String> {
    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for name in names {
        let folded = name.to_lowercase();
        match seen.entry(folded) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(name);
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if slot.get().chars().any(char::is_uppercase) && !name.chars().any(char::is_uppercase)
                {
                    slot.insert(name);
                }
            }
        }
    }
    seen.into_values().collect()
}

/// `configForDisplay()`'s safe counterpart for the unauthenticated `/api/config`.
///
/// This deliberately diverges from maw-js, which returns the whole config. A
/// denylist on a public endpoint fails open: an unusual auth-bearing key leaks
/// until someone guesses its name. Build only the fields maw-ui-lite reads, then
/// redact that smaller tree as a second line of defense.
fn serveconfig_for_display(config: &serde_json::Value) -> serde_json::Value {
    let env_masked = serveconfig_masked_env(config);
    let mut display = serde_json::Value::Object(serde_json::Map::new());
    if let Some(map) = display.as_object_mut() {
        for key in [
            "node",
            "agents",
            "sessions",
            "commands",
            "namedPeers",
            "version",
            "registryVersion",
        ] {
            if let Some(value) = config.get(key) {
                map.insert(key.to_owned(), value.clone());
            }
        }
    }
    serveconfig_strip_url_userinfo(&mut display);
    config_redact_value(&mut display);
    if let Some(map) = display.as_object_mut() {
        map.insert(
            "env".to_owned(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
        map.insert("envMasked".to_owned(), serde_json::Value::Object(env_masked));
    }
    display
}

fn serveconfig_strip_url_userinfo(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            if let Some(stripped) = serveconfig_url_without_userinfo(text) {
                *text = stripped;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                serveconfig_strip_url_userinfo(item);
            }
        }
        serde_json::Value::Object(map) => {
            for child in map.values_mut() {
                serveconfig_strip_url_userinfo(child);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn serveconfig_url_without_userinfo(value: &str) -> Option<String> {
    let scheme_end = value.find("://")?;
    if !serveconfig_valid_url_scheme(&value[..scheme_end]) {
        return None;
    }
    let authority_start = scheme_end + "://".len();
    let rest = &value[authority_start..];
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_len];
    let userinfo_end = authority.rfind('@')?;
    let host = &authority[userinfo_end + 1..];
    if host.is_empty() {
        return None;
    }
    let mut stripped = String::with_capacity(value.len().saturating_sub(userinfo_end + 1));
    stripped.push_str(&value[..authority_start]);
    stripped.push_str(host);
    stripped.push_str(&rest[authority_len..]);
    Some(stripped)
}

fn serveconfig_valid_url_scheme(value: &str) -> bool {
    let Some(first) = value.chars().next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && value
            .chars()
            .skip(1)
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
}

fn serveconfig_masked_env(config: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut masked = serde_json::Map::new();
    let Some(env) = config.get("env").and_then(serde_json::Value::as_object) else {
        return masked;
    };
    for (key, value) in env {
        masked.insert(key.clone(), config_mask_secret(value));
    }
    masked
}

#[cfg(test)]
mod serve_oracle_registry_tests {
    use super::*;

    fn registry_config() -> serde_json::Value {
        serde_json::json!({
            "agents": { "atlas": "local", "hound-oracle": "local", "default": "local", "_scratch": "local" },
            "sessions": { "cipher": "local" },
            "commands": { "ls": "ls -la", "prism-oracle": "maw wake prism" },
            "env": { "OPENAI_API_KEY": "sk-abcdefghijklmnop", "SHORT": "ab" },
            "federationToken": "tok-abcdefghijklmnop",
            "node": "local"
        })
    }

    #[test]
    fn serveoracles_lists_agents_sessions_and_oracle_commands() {
        let payload = serveoracles_payload_from_config(&registry_config());
        let oracles = payload["oracles"].as_array().expect("oracles array");
        let names = oracles
            .iter()
            .map(|value| value.as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["atlas", "cipher", "hound", "prism"]);
        assert_eq!(payload["count"], 4);
    }

    #[test]
    fn serveoracles_drops_placeholder_names_and_plain_commands() {
        let payload = serveoracles_payload_from_config(&registry_config());
        let names = payload["oracles"].to_string();
        // `default` and `_scratch` are what the client's canonicalOracleName
        // discards; `ls` is a shell alias, not an oracle.
        assert!(!names.contains("default"), "{names}");
        assert!(!names.contains("_scratch"), "{names}");
        assert!(!names.contains("\"ls\""), "{names}");
    }

    #[test]
    fn serveoracles_folds_bare_and_suffixed_spellings_together() {
        let config = serde_json::json!({
            "agents": { "atlas": "local", "atlas-oracle": "local", "ATLAS-ORACLE": "local" }
        });
        let payload = serveoracles_payload_from_config(&config);
        assert_eq!(payload["count"], 1);
        assert_eq!(payload["oracles"][0], "atlas");
    }

    #[test]
    fn serveoracles_returns_empty_roster_without_panicking() {
        let payload = serveoracles_payload_from_config(&serde_json::json!({}));
        assert_eq!(payload["count"], 0);
        assert_eq!(payload["oracles"], serde_json::json!([]));
    }

    #[test]
    fn serveconfig_keeps_agents_a_map_for_the_client_fallback() {
        let config = registry_config();
        let display = serveconfig_for_display(&config);
        // namesFromConfig walks Object.keys(payload.agents); an array here
        // yields an empty roster and a silently dead fallback.
        assert!(display["agents"].is_object());
        assert!(display["sessions"].is_object());
        assert!(display["commands"].is_object());
        assert_eq!(display["node"], "local");
    }

    #[test]
    fn serveconfig_withholds_env_values_and_federation_token() {
        let config = registry_config();
        let display = serveconfig_for_display(&config);
        let rendered = display.to_string();
        assert!(!rendered.contains("sk-abcdefghijklmnop"), "{rendered}");
        assert!(!rendered.contains("tok-abcdefghijklmnop"), "{rendered}");
        assert_eq!(
            display["env"],
            serde_json::Value::Object(serde_json::Map::new())
        );
        assert!(display["envMasked"]["OPENAI_API_KEY"].is_string());
        assert_eq!(display["envMasked"]["SHORT"], "****");
    }

    #[test]
    fn serveconfig_allowlist_drops_auth_bearing_plugin_headers() {
        let config = serde_json::json!({
            "node": "local",
            "agents": {"atlas": "local"},
            "sessions": {},
            "commands": {},
            "plugins": {
                "webhook": {
                    "headers": {
                        "Authorization": "Bearer LEAKCANARY-auth-123456",
                        "Cookie": "sid=LEAKCANARY-cookie-abcdef"
                    },
                    "apiKey": "LEAKCANARY-should-be-masked"
                }
            }
        });
        let display = serveconfig_for_display(&config);
        let rendered = display.to_string();
        assert!(!rendered.contains("LEAKCANARY-auth-123456"), "{rendered}");
        assert!(!rendered.contains("LEAKCANARY-cookie-abcdef"), "{rendered}");
        assert!(!rendered.contains("LEAKCANARY-should-be-masked"), "{rendered}");
        assert!(display.get("plugins").is_none(), "{display}");
        assert!(display["agents"].is_object());
    }

    #[test]
    fn serveconfig_named_peers_strips_url_userinfo_canaries() {
        let config = serde_json::json!({
            "node": "local",
            "agents": {"atlas": "local"},
            "namedPeers": {
                "m5": {"url": "http://admin:LEAKCANARY-peerpw@m5.local:3456"},
                "flat": "https://svc:LEAKCANARY-flatpw@other.local:3456"
            }
        });
        let display = serveconfig_for_display(&config);
        let rendered = display.to_string();
        assert!(!rendered.contains("LEAKCANARY-peerpw"), "{rendered}");
        assert!(!rendered.contains("LEAKCANARY-flatpw"), "{rendered}");
        assert_eq!(
            display["namedPeers"]["m5"]["url"],
            "http://m5.local:3456"
        );
        assert_eq!(
            display["namedPeers"]["flat"],
            "https://other.local:3456"
        );
    }

    #[test]
    fn serveoracles_real_loader_ignores_fleet_windows() {
        let _lock = env_test_lock();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "maw-rs-serveoracles-fleet-{}-{unique}",
            std::process::id()
        ));
        let _home = EnvVarRestore::capture("HOME");
        let _maw_home = EnvVarRestore::capture("MAW_HOME");
        let _config = EnvVarRestore::capture("MAW_CONFIG_DIR");
        let config_dir = root.join("config");
        let fleet_dir = root.join("home/.maw/fleet");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&fleet_dir).expect("fleet dir");
        std::fs::write(
            config_dir.join("maw.config.json"),
            r#"{"node":"m5","agents":{"atlas":"remote"},"sessions":{},"commands":{}}"#,
        )
        .expect("config");
        // A stale fleet registration (e.g. a team member auto-registered by
        // `maw wake`, its repo since deleted) must never leak into the
        // roster: config is the only source of truth for both endpoints now.
        std::fs::write(
            fleet_dir.join("t4532-summon.json"),
            r#"{"name":"t4532-summon","windows":[{"name":"coder-serve-oracle","repo":"arnon2020/maw-rs"},{"name":"verifier-x-oracle","repo":"arnon2020/maw-rs"},{"name":"claim-verifier","repo":"arnon2020/lucifer-oracle"}]}"#,
        )
        .expect("fleet");
        std::env::set_var("HOME", root.join("home"));
        std::env::remove_var("MAW_HOME");
        std::env::set_var("MAW_CONFIG_DIR", &config_dir);

        let roster = serveoracles_http_payload_read_only().expect("roster");
        let names = roster["oracles"].to_string();
        assert!(names.contains("atlas"), "{names}");
        assert!(!names.contains("coder-serve"), "{names}");
        assert!(!names.contains("verifier-x"), "{names}");
        assert!(!names.contains("claim-verifier"), "{names}");

        let display = serveconfig_http_payload_read_only().expect("config display");
        assert_eq!(display["agents"]["atlas"], "remote");
        assert!(display["agents"].get("coder-serve-oracle").is_none());
        assert!(display["agents"].get("verifier-x-oracle").is_none());
        assert!(display["agents"].get("claim-verifier").is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn serveoracles_real_loader_reports_corrupt_config() {
        let _lock = env_test_lock();
        let root = std::env::temp_dir().join(format!(
            "maw-rs-serveoracles-corrupt-{}",
            std::process::id()
        ));
        let _home = EnvVarRestore::capture("HOME");
        let _maw_home = EnvVarRestore::capture("MAW_HOME");
        let _config = EnvVarRestore::capture("MAW_CONFIG_DIR");
        let config_dir = root.join("config");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(config_dir.join("maw.config.json"), "{ invalid json").expect("config");
        std::env::set_var("HOME", root.join("home"));
        std::env::remove_var("MAW_HOME");
        std::env::set_var("MAW_CONFIG_DIR", &config_dir);

        let error = serveoracles_http_payload_read_only().expect_err("corrupt config should fail");
        assert!(error.contains("failed to parse config JSON"), "{error}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
