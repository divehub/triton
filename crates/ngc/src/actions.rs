//! The action request envelope of the runner (`POST /api/action`, `run_emulator.py`).
//!
//! The HTTP handler of the runner accepts bodies of 1..1023 bytes, parses them as JSON and calls
//! `Emulator.action(payload["action"], payload)`. [`parse_request`] performs the same checks with the runner's
//! messages; the actions themselves (their payload fields and validation order) live in `session.rs`, line by line
//! after `Emulator.action`.

use emu_core::Json;

/// The runner refuses bodies of 1024 bytes and more (and empty ones): `Invalid request length`.
pub const MAX_REQUEST_BYTES: usize = 1024;

/// Every action name of the runner.
pub const ACTIONS: [&str; 13] =
    ["pause", "resume", "step", "advance", "reset", "cold", "wake", "up", "down", "confirm", "can", "inputs", "led-colors"];

/// Actions of the runner that are not in [`ACTIONS`] (kept separate so the table above stays a plain list of the
/// transport-independent controls).
pub const MORE_ACTIONS: [&str; 2] = ["serial", "capture"];

/// A parsed request: the action name (`Json::Str` for a valid one) and the whole payload object.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub action: Json,
    pub payload: Json,
}

impl Request {
    /// The action name, if the request carries a string.
    pub fn name(&self) -> Option<&str> {
        self.action.as_str()
    }
}

/// The envelope checks of the HTTP handler and `payload["action"]`.
pub fn parse_request(text: &str) -> Result<Request, String> {
    if text.is_empty() || text.len() >= MAX_REQUEST_BYTES {
        return Err("Invalid request length".to_string());
    }
    let payload = Json::parse(text).map_err(|e| e.to_string())?;
    let action = match &payload {
        Json::Object(_) => payload.get("action").cloned().ok_or_else(|| "'action'".to_string())?,
        Json::Array(_) => return Err("list indices must be integers or slices, not str".to_string()),
        Json::Str(_) => return Err("string indices must be integers, not 'str'".to_string()),
        Json::Null => return Err("'NoneType' object is not subscriptable".to_string()),
        Json::Bool(_) => return Err("'bool' object is not subscriptable".to_string()),
        Json::Int(_) | Json::UInt(_) => return Err("'int' object is not subscriptable".to_string()),
        Json::Float(_) => return Err("'float' object is not subscriptable".to_string()),
    };
    Ok(Request { action, payload })
}

/// True for the action names the runner knows.
pub fn is_known(name: &str) -> bool {
    ACTIONS.contains(&name) || MORE_ACTIONS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_checks_follow_the_http_handler() {
        let request = parse_request("{\"action\": \"advance\", \"seconds\": 2}").unwrap();
        assert_eq!(request.name(), Some("advance"));
        assert_eq!(request.payload.get("seconds").and_then(Json::as_f64), Some(2.0));
        assert_eq!(parse_request("").unwrap_err(), "Invalid request length");
        assert_eq!(parse_request(&format!("{{\"action\": \"{}\"}}", "x".repeat(1100))).unwrap_err(), "Invalid request length");
        assert_eq!(parse_request("{}").unwrap_err(), "'action'");
        assert!(parse_request("[1]").unwrap_err().contains("list indices"));
        assert!(parse_request("{not json").is_err());
        // A non-string action parses (the runner answers it with `Unknown action`).
        assert_eq!(parse_request("{\"action\": 5}").unwrap().name(), None);
        assert!(is_known("led-colors") && is_known("serial") && is_known("capture") && !is_known("fly"));
        assert_eq!(ACTIONS.len() + MORE_ACTIONS.len(), 15);
    }
}
