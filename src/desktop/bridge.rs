//! Validated command envelope between the bundled WebView and Rust services.
use anyhow::{Result, ensure};
use moasearch::commands::Command;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_MESSAGE_BYTES: usize = 32 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: u64,
    pub session: String,
    command: String,
    args: Value,
}
impl Request {
    pub fn parse(body: &str) -> Result<Self> {
        ensure!(body.len() <= MAX_MESSAGE_BYTES, "앱 명령이 너무 큽니다.");
        let request: Self = serde_json::from_str(body)?;
        ensure!(
            (1..=9_007_199_254_740_991).contains(&request.id),
            "잘못된 요청 번호입니다."
        );
        ensure!(
            !request.session.is_empty()
                && request.session.len() <= 128
                && request
                    .session
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_:".contains(&c)),
            "잘못된 화면 세션입니다."
        );
        Ok(request)
    }
    pub fn command(&self) -> Result<Command> {
        Ok(serde_json::from_value(
            json!({"command":self.command,"args":self.args}),
        )?)
    }
    pub fn response(&self, result: Result<Value>) -> Response {
        let (ok, value, error) = match result {
            Ok(value) => (true, value, None),
            Err(error) => (false, Value::Null, Some(error.to_string())),
        };
        Response {
            id: self.id,
            session: self.session.clone(),
            ok,
            value,
            error,
        }
    }
}

#[derive(Serialize)]
pub struct Response {
    id: u64,
    session: String,
    ok: bool,
    value: Value,
    error: Option<String>,
}
impl Response {
    pub fn script(&self) -> Result<String> {
        Ok(format!(
            "window.__moaResolve && window.__moaResolve({});",
            serde_json::to_string(self)?
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_are_typed_bounded_and_responses_are_json_escaped() -> Result<()> {
        let request =
            Request::parse(r#"{"id":1,"session":"page-1","command":"status","args":{}}"#)?;
        assert!(matches!(request.command()?, Command::Status {}));
        let request =
            Request::parse(r#"{"id":2,"session":"page-1","command":"execute_shell","args":{}}"#)?;
        assert!(request.command().is_err());
        for body in [
            r#"{"id":0,"session":"a","command":"status","args":{}}"#,
            r#"{"id":1,"session":"');evil()","command":"status","args":{}}"#,
            r#"{"id":1,"session":"a","command":"status","args":{},"extra":true}"#,
        ] {
            assert!(Request::parse(body).is_err());
        }
        assert!(Request::parse(&" ".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
        let text = "\"});evil();//\n한글";
        let response = request.response(Ok(json!({"text":text}))).script()?;
        let body = response
            .strip_prefix("window.__moaResolve && window.__moaResolve(")
            .unwrap()
            .strip_suffix(");")
            .unwrap();
        let value: Value = serde_json::from_str(body)?;
        assert_eq!(value["value"]["text"], text);
        assert_eq!(value["session"], "page-1");
        Ok(())
    }
}
