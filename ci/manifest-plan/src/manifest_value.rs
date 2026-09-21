use std::collections::BTreeMap;
use std::ops::Index;
use std::str::FromStr;

use serde::Serialize;

/// The deliberately small data model accepted by the E2E manifest schema.
///
/// Keeping this independent of the serialization format lets the validator and
/// execution code consume one representation while the checked-in manifests
/// use YAML for readable nested mode/backend policy.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Value {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Array(Vec<Value>),
    Table(BTreeMap<String, Value>),
    Null,
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Self> {
        self.as_table()?.get(key)
    }

    pub fn as_table(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Self::Table(table) => Some(table),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Self::Array(array) => Some(array),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(value) => Some(*value),
            _ => None,
        }
    }
}

impl Index<&str> for Value {
    type Output = Value;

    fn index(&self, key: &str) -> &Self::Output {
        self.get(key)
            .unwrap_or_else(|| panic!("missing manifest key `{key}`"))
    }
}

impl FromStr for Value {
    type Err = String;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        let yaml: serde_yaml::Value =
            serde_yaml::from_str(source).map_err(|error| error.to_string())?;
        convert(yaml)
    }
}

fn convert(value: serde_yaml::Value) -> Result<Value, String> {
    match value {
        serde_yaml::Value::Null => Ok(Value::Null),
        serde_yaml::Value::Bool(value) => Ok(Value::Boolean(value)),
        serde_yaml::Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Ok(Value::Integer(integer))
            } else if let Some(float) = value.as_f64() {
                Ok(Value::Float(float))
            } else {
                Err(format!("unsupported YAML number: {value}"))
            }
        }
        serde_yaml::Value::String(value) => Ok(Value::String(value)),
        serde_yaml::Value::Sequence(values) => values
            .into_iter()
            .map(convert)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        serde_yaml::Value::Mapping(values) => {
            let mut table = BTreeMap::new();
            for (key, value) in values {
                let serde_yaml::Value::String(key) = key else {
                    return Err("manifest mapping keys must be strings".to_string());
                };
                if table.insert(key.clone(), convert(value)?).is_some() {
                    return Err(format!("duplicate manifest key `{key}`"));
                }
            }
            Ok(Value::Table(table))
        }
        serde_yaml::Value::Tagged(_) => Err("YAML tags are not allowed in manifests".to_string()),
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub fn from_toml(value: toml::Value) -> Value {
    match value {
        toml::Value::String(value) => Value::String(value),
        toml::Value::Integer(value) => Value::Integer(value),
        toml::Value::Float(value) => Value::Float(value),
        toml::Value::Boolean(value) => Value::Boolean(value),
        toml::Value::Datetime(value) => Value::String(value.to_string()),
        toml::Value::Array(values) => Value::Array(values.into_iter().map(from_toml).collect()),
        toml::Value::Table(values) => Value::Table(
            values
                .into_iter()
                .map(|(key, value)| (key, from_toml(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::Value;

    #[test]
    fn parses_nested_backend_reasons() {
        let value: Value = r#"
test:
  modes:
    verify:
      ci: false
      backends_enabled: [ptrace, sabre]
      backends_disabled:
        dbt: reason
        kvm: reason
        liteinst: reason
"#
        .parse()
        .unwrap();
        assert_eq!(
            value
                .get("test")
                .and_then(|value| value.get("modes"))
                .and_then(|value| value.get("verify"))
                .and_then(|value| value.get("backends_disabled"))
                .and_then(|value| value.get("kvm"))
                .and_then(Value::as_str),
            Some("reason")
        );
    }

    #[test]
    fn rejects_non_string_mapping_keys() {
        assert!("1: value".parse::<Value>().is_err());
    }
}
