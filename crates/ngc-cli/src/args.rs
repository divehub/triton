//! Tiny hand-written argument parser (the workspace has no external crates).
//!
//! Supports `--name value`, `--name=value`, boolean flags and positional arguments. Unknown
//! options and missing values are errors with a message naming the option.

use std::collections::HashMap;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    values: HashMap<String, String>,
    flags: Vec<String>,
    pub positional: Vec<String>,
}

/// Parses `args` against the declared value options (`--out-dir DIR`) and flags (`--json`).
pub fn parse(args: &[String], value_options: &[&str], flag_options: &[&str]) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        let Some(name) = arg.strip_prefix("--") else {
            parsed.positional.push(arg.clone());
            continue;
        };
        let (name, inline) = match name.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (name, None),
        };
        if flag_options.contains(&name) {
            if inline.is_some() {
                return Err(format!("option --{name} does not take a value"));
            }
            if !parsed.flags.iter().any(|f| f == name) {
                parsed.flags.push(name.to_string());
            }
        } else if value_options.contains(&name) {
            let value = match inline {
                Some(v) => v,
                None => {
                    let v = args.get(i).ok_or_else(|| format!("option --{name} needs a value"))?;
                    i += 1;
                    v.clone()
                }
            };
            if parsed.values.insert(name.to_string(), value).is_some() {
                return Err(format!("option --{name} given more than once"));
            }
        } else {
            return Err(format!("unknown option --{name}"));
        }
    }
    Ok(parsed)
}

impl Args {
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn require(&self, name: &str) -> Result<&str, String> {
        self.value(name).ok_or_else(|| format!("missing required option --{name}"))
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_values_flags_and_positionals() {
        let a = parse(&strings(&["--main", "a.srec", "--handset=b.srec", "--json", "extra"]), &["main", "handset"], &["json"]).unwrap();
        assert_eq!(a.value("main"), Some("a.srec"));
        assert_eq!(a.value("handset"), Some("b.srec"));
        assert!(a.flag("json"));
        assert!(!a.flag("vectors"));
        assert_eq!(a.positional, ["extra"]);
        assert_eq!(a.require("main"), Ok("a.srec"));
        assert!(a.require("out-dir").unwrap_err().contains("--out-dir"));
    }

    #[test]
    fn rejects_bad_input() {
        let value = ["main"];
        let flag = ["json"];
        assert!(parse(&strings(&["--nope"]), &value, &flag).unwrap_err().contains("unknown option --nope"));
        assert!(parse(&strings(&["--main"]), &value, &flag).unwrap_err().contains("needs a value"));
        assert!(parse(&strings(&["--json=1"]), &value, &flag).unwrap_err().contains("does not take a value"));
        assert!(parse(&strings(&["--main", "a", "--main", "b"]), &value, &flag).unwrap_err().contains("more than once"));
        // A value may look like an option.
        let a = parse(&strings(&["--main", "--weird"]), &value, &flag).unwrap();
        assert_eq!(a.value("main"), Some("--weird"));
    }
}
