//! JSON-RPC 2.0 消息：请求（带 id）、通知（不带 id）、响应（成功或错误）。

use std::io::{Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::frame::{FrameError, read_frame, write_frame};

pub const JSONRPC_VERSION: &str = "2.0";

/// 规范定义的错误码；-32000 到 -32099 留给各进程自定义。
pub mod error_code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const SERVER_ERROR: i64 = -32000;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(u64),
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Id,
    pub method: String,
    /// 没有参数时为 `Value::Null`。
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: Id,
    pub outcome: Result<Value, ErrorObject>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}（错误码 {code}）")]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

impl Request {
    pub fn new(id: Id, method: impl Into<String>, params: Value) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }

    /// 把参数解析成 `T`；不符合时返回可以直接答复给对端的「参数无效」错误。
    pub fn params<T: DeserializeOwned>(&self) -> Result<T, ErrorObject> {
        serde_json::from_value(self.params.clone()).map_err(|error| {
            ErrorObject::new(
                error_code::INVALID_PARAMS,
                format!("{} 的参数不对：{error}", self.method),
            )
        })
    }
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            method: method.into(),
            params,
        }
    }
}

impl ErrorObject {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl Message {
    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        object.insert("jsonrpc".to_owned(), JSONRPC_VERSION.into());
        match self {
            Self::Request(request) => {
                object.insert("id".to_owned(), id_value(&request.id));
                object.insert("method".to_owned(), request.method.clone().into());
                if !request.params.is_null() {
                    object.insert("params".to_owned(), request.params.clone());
                }
            }
            Self::Notification(notification) => {
                object.insert("method".to_owned(), notification.method.clone().into());
                if !notification.params.is_null() {
                    object.insert("params".to_owned(), notification.params.clone());
                }
            }
            Self::Response(response) => {
                object.insert("id".to_owned(), id_value(&response.id));
                match &response.outcome {
                    Ok(result) => object.insert("result".to_owned(), result.clone()),
                    Err(error) => object.insert("error".to_owned(), error_value(error)),
                };
            }
        }
        Value::Object(object)
    }

    pub fn from_value(value: Value) -> Result<Self, String> {
        let Value::Object(mut object) = value else {
            return Err("消息不是 JSON 对象".to_owned());
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            return Err("缺少 \"jsonrpc\": \"2.0\"".to_owned());
        }
        let id = match object.remove("id") {
            None | Some(Value::Null) => None,
            Some(id) => Some(
                serde_json::from_value::<Id>(id).map_err(|_| "id 必须是数字或字符串".to_owned())?,
            ),
        };
        let params = object.remove("params").unwrap_or(Value::Null);

        if let Some(method) = object.remove("method") {
            let Value::String(method) = method else {
                return Err("method 必须是字符串".to_owned());
            };
            return Ok(match id {
                Some(id) => Self::Request(Request { id, method, params }),
                None => Self::Notification(Notification { method, params }),
            });
        }

        let id = id.ok_or_else(|| "响应缺少 id".to_owned())?;
        let outcome = match (object.remove("result"), object.remove("error")) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(serde_json::from_value::<ErrorObject>(error)
                .map_err(|_| "error 字段格式不对".to_owned())?),
            _ => return Err("响应必须且只能带 result 或 error 之一".to_owned()),
        };
        Ok(Self::Response(Response { id, outcome }))
    }
}

pub fn write_message<W: Write>(writer: &mut W, message: &Message) -> Result<(), FrameError> {
    write_frame(writer, &message.to_value())
}

pub fn read_message<R: Read>(reader: &mut R) -> Result<Message, FrameError> {
    let value = read_frame::<_, Value>(reader)?;
    Message::from_value(value).map_err(FrameError::Invalid)
}

fn id_value(id: &Id) -> Value {
    match id {
        Id::Number(number) => (*number).into(),
        Id::Text(text) => text.clone().into(),
    }
}

fn error_value(error: &ErrorObject) -> Value {
    let mut object = Map::new();
    object.insert("code".to_owned(), error.code.into());
    object.insert("message".to_owned(), error.message.clone().into());
    if let Some(data) = &error.data {
        object.insert("data".to_owned(), data.clone());
    }
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::*;

    fn round_trip(message: Message) {
        let mut wire = Vec::new();
        write_message(&mut wire, &message).unwrap();
        assert_eq!(read_message(&mut Cursor::new(wire)).unwrap(), message);
    }

    #[test]
    fn every_kind_round_trips() {
        round_trip(Message::Request(Request::new(
            Id::Number(7),
            "disk.readScan",
            json!({"jobId": "j1"}),
        )));
        round_trip(Message::Request(Request::new(
            Id::Text("a".to_owned()),
            "engine.capabilities",
            Value::Null,
        )));
        round_trip(Message::Notification(Notification::new(
            "job.progress",
            json!({"done": 1, "total": 2}),
        )));
        round_trip(Message::Response(Response {
            id: Id::Number(7),
            outcome: Ok(json!({})),
        }));
        round_trip(Message::Response(Response {
            id: Id::Number(8),
            outcome: Err(ErrorObject::new(error_code::METHOD_NOT_FOUND, "未知方法")
                .with_data(json!({"osError": 5}))),
        }));
    }

    #[test]
    fn wire_format_follows_the_specification() {
        let request = Message::Request(Request::new(
            Id::Number(1),
            "job.cancel",
            json!({"jobId": "j"}),
        ));
        assert_eq!(
            request.to_value(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "job.cancel", "params": {"jobId": "j"}})
        );
        let notification =
            Message::Notification(Notification::new("engine.heartbeat", Value::Null));
        assert_eq!(
            notification.to_value(),
            json!({"jsonrpc": "2.0", "method": "engine.heartbeat"})
        );
        let error = Message::Response(Response {
            id: Id::Number(2),
            outcome: Err(ErrorObject::new(error_code::INVALID_PARAMS, "x")),
        });
        assert_eq!(
            error.to_value(),
            json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32602, "message": "x"}})
        );
    }

    #[test]
    fn null_result_is_still_a_success() {
        let message =
            Message::from_value(json!({"jsonrpc": "2.0", "id": 3, "result": null})).unwrap();
        assert_eq!(
            message,
            Message::Response(Response {
                id: Id::Number(3),
                outcome: Ok(Value::Null)
            })
        );
    }

    #[test]
    fn malformed_messages_are_rejected() {
        for value in [
            json!([1, 2]),
            json!({"id": 1, "method": "x"}),
            json!({"jsonrpc": "1.0", "id": 1, "method": "x"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": 5}),
            json!({"jsonrpc": "2.0", "id": true, "method": "x"}),
            json!({"jsonrpc": "2.0", "result": 1}),
            json!({"jsonrpc": "2.0", "id": 1}),
            json!({"jsonrpc": "2.0", "id": 1, "result": 1, "error": {"code": 1, "message": "x"}}),
            json!({"jsonrpc": "2.0", "id": 1, "error": "x"}),
        ] {
            assert!(Message::from_value(value.clone()).is_err(), "{value}");
        }
    }

    #[test]
    fn invalid_params_become_an_error_object() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Params {
            path: String,
        }
        let request = Request::new(Id::Number(1), "disk.readScan", json!({"path": 3}));
        let error = request.params::<Params>().unwrap_err();
        assert_eq!(error.code, error_code::INVALID_PARAMS);
        assert!(error.message.starts_with("disk.readScan 的参数不对"));
    }
}
