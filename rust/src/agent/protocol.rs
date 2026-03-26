use serde_json::Value;

pub const INITIALIZE_ID: u64 = 1;
pub const THREAD_START_ID: u64 = 2;
pub const FIRST_TURN_START_ID: u64 = 3;
pub const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

pub fn thread_id_from_response(payload: &Value) -> Option<String> {
    payload
        .get("result")
        .and_then(|result| result.get("thread"))
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub fn turn_id_from_response(payload: &Value) -> Option<String> {
    payload
        .get("result")
        .and_then(|result| result.get("turn"))
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub fn usage_from_event(payload: &Value) -> Option<Value> {
    payload
        .get("params")
        .and_then(|params| params.get("usage"))
        .cloned()
        .or_else(|| {
            payload
                .get("result")
                .and_then(|result| result.get("usage"))
                .cloned()
        })
}

pub fn rate_limits_from_event(payload: &Value) -> Option<Value> {
    payload
        .get("params")
        .and_then(|params| params.get("rateLimits"))
        .cloned()
        .or_else(|| {
            payload
                .get("result")
                .and_then(|result| result.get("rateLimits"))
                .cloned()
        })
}

pub fn tool_name(payload: &Value) -> Option<&str> {
    payload
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("params")
                .and_then(|params| params.get("tool"))
                .and_then(Value::as_str)
        })
}
