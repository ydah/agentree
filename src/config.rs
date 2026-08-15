use std::{collections::BTreeMap, ffi::OsString, path::Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    domain::{AppError, ErrorKind, InternalGitProfile},
    git::GitRunner,
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigSnapshot {
    pub hash: String,
    pub source_oid: String,
    pub checks: Vec<CheckDefinition>,
    pub raw: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckDefinition {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default = "default_check_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_check_output_limit_bytes")]
    pub output_limit_bytes: usize,
}

fn default_check_timeout_seconds() -> u64 {
    1800
}

fn default_check_output_limit_bytes() -> usize {
    8 * 1024 * 1024
}

pub fn snapshot(git: &GitRunner, root: &Path, base_oid: &str) -> Result<ConfigSnapshot, AppError> {
    let raw = match git.run(
        root,
        InternalGitProfile::Discovery,
        &[
            OsString::from("show"),
            OsString::from(format!("{base_oid}:.agentree.toml")),
        ],
    ) {
        Ok(output) => String::from_utf8(output.stdout).map_err(|_| {
            AppError::diagnostic(
                "AGT-0401",
                "project config is not UTF-8",
                ErrorKind::Unsupported,
            )
        })?,
        Err(_) => String::new(),
    };
    let parsed: toml::Value = if raw.trim().is_empty() {
        toml::Value::Table(Default::default())
    } else {
        raw.parse()?
    };
    let json = serde_json::to_value(&parsed)
        .map_err(|error| AppError::diagnostic("AGT-0402", error.to_string(), ErrorKind::Usage))?;
    let mut table = match json {
        serde_json::Value::Object(map) => map.into_iter().collect::<BTreeMap<_, _>>(),
        _ => BTreeMap::new(),
    };
    let checks = parse_checks(table.get("checks"))?;
    let canonical = serde_json::to_vec(&table)?;
    let mut hasher = Sha256::new();
    hasher.update(canonical);
    let hash = format!("sha256:{:x}", hasher.finalize());
    Ok(ConfigSnapshot {
        hash,
        source_oid: base_oid.to_owned(),
        checks,
        raw: std::mem::take(&mut table),
    })
}

fn parse_checks(value: Option<&serde_json::Value>) -> Result<Vec<CheckDefinition>, AppError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(AppError::diagnostic(
            "AGT-0403",
            "checks must be an array",
            ErrorKind::Usage,
        ));
    };
    items
        .iter()
        .map(|item| {
            let object = item.as_object().ok_or_else(|| {
                AppError::diagnostic("AGT-0404", "each check must be a table", ErrorKind::Usage)
            })?;
            let name = object
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    AppError::diagnostic("AGT-0405", "check.name is required", ErrorKind::Usage)
                })?
                .to_owned();
            let command = object
                .get("command")
                .or_else(|| object.get("argv"))
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    AppError::diagnostic(
                        "AGT-0406",
                        "check.command or check.argv must be an argv array",
                        ErrorKind::Usage,
                    )
                })?
                .iter()
                .map(|part| {
                    part.as_str().map(str::to_owned).ok_or_else(|| {
                        AppError::diagnostic(
                            "AGT-0407",
                            "check command arguments must be strings",
                            ErrorKind::Usage,
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if command.is_empty() {
                return Err(AppError::diagnostic(
                    "AGT-0408",
                    "check command cannot be empty",
                    ErrorKind::Usage,
                ));
            }
            let timeout_seconds = object
                .get("timeout_seconds")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(default_check_timeout_seconds() as i64);
            if timeout_seconds <= 0 {
                return Err(AppError::diagnostic(
                    "AGT-0409",
                    "check.timeout_seconds must be positive",
                    ErrorKind::Usage,
                ));
            }
            let output_limit_bytes = object
                .get("output_limit_bytes")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(default_check_output_limit_bytes() as i64);
            if output_limit_bytes <= 0 {
                return Err(AppError::diagnostic(
                    "AGT-0410",
                    "check.output_limit_bytes must be positive",
                    ErrorKind::Usage,
                ));
            }
            Ok(CheckDefinition {
                name,
                command,
                required: object
                    .get("required")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                timeout_seconds: timeout_seconds as u64,
                output_limit_bytes: output_limit_bytes as usize,
            })
        })
        .collect()
}

pub fn scaffold() -> &'static str {
    "# Agentree project configuration\n# Commands are argv arrays; no shell interpolation is performed.\nchecks = [\n  # { name = \"unit\", command = [\"cargo\", \"test\"], required = true },\n]\n"
}
