use std::path::PathBuf;

use serde_json::Value;

pub(crate) fn liteinst_cdylibs_from_cargo_messages(messages: &str) -> Result<Vec<PathBuf>, String> {
    let mut artifacts = Vec::new();
    for (index, line) in messages.lines().filter(|line| !line.is_empty()).enumerate() {
        let message: Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid Cargo JSON on line {}: {error}", index + 1))?;
        let target = &message["target"];
        let is_liteinst_cdylib = message["reason"] == "compiler-artifact"
            && target["name"] == "reverie_liteinst"
            && target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"));
        if !is_liteinst_cdylib {
            continue;
        }
        let filenames = message["filenames"].as_array().ok_or_else(|| {
            format!(
                "LiteInst compiler-artifact on line {} has no filenames",
                index + 1
            )
        })?;
        artifacts.extend(
            filenames
                .iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .filter(|path| path.extension().is_some_and(|extension| extension == "so")),
        );
    }
    artifacts.sort();
    artifacts.dedup();
    Ok(artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_the_current_liteinst_cdylib_message() {
        let messages = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"unrelated","kind":["cdylib"]},"filenames":["/warm/deps/libreverie_liteinst-stale.so"]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"reverie_liteinst","kind":["rlib","cdylib"]},"filenames":["/isolated/deps/libreverie_liteinst-current.rlib","/isolated/deps/libreverie_liteinst-current.so"],"fresh":true}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
        );

        assert_eq!(
            liteinst_cdylibs_from_cargo_messages(messages).unwrap(),
            [PathBuf::from(
                "/isolated/deps/libreverie_liteinst-current.so"
            )]
        );
    }

    #[test]
    fn rejects_non_json_output() {
        let error = liteinst_cdylibs_from_cargo_messages("not cargo json").unwrap_err();
        assert!(error.contains("invalid Cargo JSON on line 1"));
    }
}
