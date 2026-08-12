use crate::{refusal, Result};
use regex::Regex;
use std::collections::BTreeSet;

pub fn filter_source(
    text: &str,
    selected: &BTreeSet<String>,
    known: &BTreeSet<String>,
    source: &str,
) -> Result<String> {
    let if_re = Regex::new(
        r"^\s*(?://+|\*)\s*nmp-native:if\s+([a-z][a-z0-9]*(?:-[a-z0-9]+)*)(\s*(?:\*/)?\s*)$",
    )
    .expect("constant regex");
    let endif_re =
        Regex::new(r"^\s*(?://+|\*)\s*nmp-native:endif(\s*(?:\*/)?\s*)$").expect("constant regex");
    let mut stack: Vec<(String, bool)> = Vec::new();
    let mut output = String::new();
    for (index, inclusive) in text.split_inclusive('\n').enumerate() {
        let line = inclusive.trim_end_matches(['\r', '\n']);
        let ending = &inclusive[line.len()..];
        if let Some(capture) = if_re.captures(line) {
            let key = capture[1].to_owned();
            if !known.contains(&key) {
                return Err(refusal(format!(
                    "{source}:{}: conditional marker names unknown capability {key:?}",
                    index + 1
                )));
            }
            let parent_enabled = stack.iter().all(|(_, enabled)| *enabled);
            stack.push((key.clone(), selected.contains(&key)));
            if parent_enabled
                && capture
                    .get(2)
                    .is_some_and(|part| part.as_str().contains("*/"))
            {
                output.push_str(" * */");
                output.push_str(ending);
            }
            continue;
        }
        if let Some(capture) = endif_re.captures(line) {
            if stack.is_empty() {
                return Err(refusal(format!(
                    "{source}:{}: nmp-native:endif has no matching if",
                    index + 1
                )));
            }
            let parent_enabled = stack[..stack.len() - 1].iter().all(|(_, enabled)| *enabled);
            stack.pop();
            if parent_enabled
                && capture
                    .get(1)
                    .is_some_and(|part| part.as_str().contains("*/"))
            {
                output.push_str(" * */");
                output.push_str(ending);
            }
            continue;
        }
        if stack.iter().all(|(_, enabled)| *enabled) {
            output.push_str(inclusive);
        }
    }
    if !stack.is_empty() {
        return Err(refusal(format!(
            "{source}: unterminated conditional blocks: {}",
            stack
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(output)
}
