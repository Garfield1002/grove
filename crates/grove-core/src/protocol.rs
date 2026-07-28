//! Versioned, length-prefixed JSON protocol for the persistent Grove service.
//!
//! Each message is a four-byte big-endian payload length followed by exactly
//! that many bytes of UTF-8 JSON. The fixed header makes partial reads,
//! multiple requests, and payload bounds unambiguous without reserving
//! characters inside JSON.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current public service protocol version.
pub const VERSION: u32 = 1;
/// Largest accepted JSON payload. Snapshots need room to grow, but a local
/// client must not be able to make the service allocate without bound.
pub const MAX_PAYLOAD_LEN: usize = 1024 * 1024;
/// Request identifiers are echoed in responses and logs, never interpreted.
pub const MAX_REQUEST_ID_LEN: usize = 128;
pub const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol: u32,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: impl Into<String>, method: impl Into<String>, params: Value) -> Self {
        Self {
            protocol: VERSION,
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    pub fn validate(&self) -> std::result::Result<(), ValidationError> {
        validate_token(&self.id, MAX_REQUEST_ID_LEN, "request id")?;
        validate_token(&self.method, MAX_REQUEST_ID_LEN, "method")
    }
}

fn validate_token(
    value: &str,
    max_len: usize,
    field: &'static str,
) -> std::result::Result<(), ValidationError> {
    if value.is_empty() {
        return Err(ValidationError::Empty(field));
    }
    if value.len() > max_len {
        return Err(ValidationError::TooLong {
            field,
            max: max_len,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(ValidationError::Control(field));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("{0} must not be empty")]
    Empty(&'static str),
    #[error("{field} is longer than {max} bytes")]
    TooLong { field: &'static str, max: usize },
    #[error("{0} contains a control character")]
    Control(&'static str),
    #[error("response must contain exactly one of result or error, matching ok")]
    ResponseShape,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub protocol: u32,
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

impl Response {
    pub fn success(id: impl Into<String>, result: Value) -> Self {
        Self {
            protocol: VERSION,
            id: id.into(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            protocol: VERSION,
            id: id.into(),
            ok: false,
            result: None,
            error: Some(ResponseError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }

    pub fn validate(&self) -> std::result::Result<(), ValidationError> {
        validate_token(&self.id, MAX_REQUEST_ID_LEN, "response id")?;
        match (self.ok, self.result.is_some(), self.error.is_some()) {
            (true, true, false) | (false, false, true) => Ok(()),
            _ => Err(ValidationError::ResponseShape),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("service message payload is empty")]
    Empty,
    #[error("service message is {actual} bytes; maximum is {maximum}")]
    TooLarge { actual: usize, maximum: usize },
    #[error("service message is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid service request: {0}")]
    Validation(#[from] ValidationError),
    #[error("service responded with protocol {actual}; client speaks {expected}")]
    ProtocolVersion { actual: u32, expected: u32 },
}

fn io(context: &'static str, source: std::io::Error) -> Error {
    Error::Io { context, source }
}

pub fn configure(stream: &UnixStream) -> Result<(), Error> {
    configure_with_timeout(stream, IO_TIMEOUT)
}

fn configure_with_timeout(stream: &UnixStream, timeout: Duration) -> Result<(), Error> {
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| io("set service read timeout", error))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| io("set service write timeout", error))
}

pub fn write_json<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<(), Error> {
    let payload = serde_json::to_vec(value)?;
    if payload.is_empty() {
        return Err(Error::Empty);
    }
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(Error::TooLarge {
            actual: payload.len(),
            maximum: MAX_PAYLOAD_LEN,
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| Error::TooLarge {
        actual: payload.len(),
        maximum: MAX_PAYLOAD_LEN,
    })?;
    stream
        .write_all(&length.to_be_bytes())
        .map_err(|error| io("write service frame header", error))?;
    stream
        .write_all(&payload)
        .map_err(|error| io("write service frame payload", error))
}

pub fn read_json<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> Result<T, Error> {
    let mut header = [0_u8; 4];
    stream
        .read_exact(&mut header)
        .map_err(|error| io("read service frame header", error))?;
    read_json_after_header(stream, header)
}

/// Read a framed value after a protocol discriminator consumed the first byte
/// of its four-byte length header.
pub fn read_json_after_first<T: for<'de> Deserialize<'de>>(
    stream: &mut UnixStream,
    first: u8,
) -> Result<T, Error> {
    let mut header = [0_u8; 4];
    header[0] = first;
    stream
        .read_exact(&mut header[1..])
        .map_err(|error| io("read service frame header", error))?;
    read_json_after_header(stream, header)
}

fn read_json_after_header<T: for<'de> Deserialize<'de>>(
    stream: &mut UnixStream,
    header: [u8; 4],
) -> Result<T, Error> {
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 {
        return Err(Error::Empty);
    }
    if length > MAX_PAYLOAD_LEN {
        return Err(Error::TooLarge {
            actual: length,
            maximum: MAX_PAYLOAD_LEN,
        });
    }
    let mut payload = vec![0_u8; length];
    stream
        .read_exact(&mut payload)
        .map_err(|error| io("read service frame payload", error))?;
    Ok(serde_json::from_slice(&payload)?)
}

pub fn read_request(stream: &mut UnixStream) -> Result<Request, Error> {
    let request: Request = read_json(stream)?;
    request.validate()?;
    Ok(request)
}

pub fn read_request_after_first(stream: &mut UnixStream, first: u8) -> Result<Request, Error> {
    let request: Request = read_json_after_first(stream, first)?;
    request.validate()?;
    Ok(request)
}

pub fn write_response(stream: &mut UnixStream, response: &Response) -> Result<(), Error> {
    write_json(stream, response)
}

/// Send one request and wait for its matching response.
pub fn call(socket: &Path, request: &Request) -> Result<Response, Error> {
    call_with_timeout(socket, request, IO_TIMEOUT)
}

/// Send one request with a caller-selected response timeout. Long-running
/// service methods such as reconciliation use this without weakening the
/// short default for interactive probes and list calls.
pub fn call_with_timeout(
    socket: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Response, Error> {
    request.validate()?;
    let mut stream =
        UnixStream::connect(socket).map_err(|error| io("connect to Grove service", error))?;
    configure_with_timeout(&stream, timeout)?;
    write_json(&mut stream, request)?;
    let response: Response = read_json(&mut stream)?;
    response.validate()?;
    if response.protocol != VERSION {
        return Err(Error::ProtocolVersion {
            actual: response.protocol,
            expected: VERSION,
        });
    }
    if response.id != request.id {
        return Err(Error::Io {
            context: "match service response",
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "response id `{}` does not match request id `{}`",
                    response.id, request.id
                ),
            ),
        });
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn framed_json_round_trips_over_a_stream() {
        let (mut writer, mut reader) = UnixStream::pair().expect("pair");
        let request = Request::new("req-1", "ping", json!({}));
        write_json(&mut writer, &request).expect("write");
        assert_eq!(
            read_request(&mut reader).expect("read"),
            request,
            "the frame carries exactly one typed JSON value"
        );
    }

    #[test]
    fn rejects_empty_and_oversized_frames_before_allocating() {
        let (mut writer, mut reader) = UnixStream::pair().expect("pair");
        writer.write_all(&0_u32.to_be_bytes()).expect("header");
        assert!(matches!(read_json::<Value>(&mut reader), Err(Error::Empty)));

        let (mut writer, mut reader) = UnixStream::pair().expect("pair");
        let too_large = u32::try_from(MAX_PAYLOAD_LEN + 1).expect("fits");
        writer.write_all(&too_large.to_be_bytes()).expect("header");
        assert!(matches!(
            read_json::<Value>(&mut reader),
            Err(Error::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_unknown_json_fields_and_bad_identifiers() {
        let (mut writer, mut reader) = UnixStream::pair().expect("pair");
        write_json(
            &mut writer,
            &json!({
                "protocol": 1,
                "id": "x",
                "method": "ping",
                "params": {},
                "surprise": true
            }),
        )
        .expect("write");
        assert!(matches!(read_request(&mut reader), Err(Error::Json(_))));

        assert!(matches!(
            Request::new("", "ping", json!({})).validate(),
            Err(ValidationError::Empty("request id"))
        ));
    }

    #[test]
    fn validates_response_shape_and_configures_timeouts() {
        let invalid = Response {
            protocol: VERSION,
            id: "one".into(),
            ok: true,
            result: None,
            error: None,
        };
        assert!(matches!(
            invalid.validate(),
            Err(ValidationError::ResponseShape)
        ));

        let (stream, _) = UnixStream::pair().expect("pair");
        configure(&stream).expect("configures");
        assert_eq!(
            stream.read_timeout().expect("read timeout"),
            Some(IO_TIMEOUT)
        );
        assert_eq!(
            stream.write_timeout().expect("write timeout"),
            Some(IO_TIMEOUT)
        );
    }

    #[test]
    fn a_stalled_partial_frame_times_out() {
        let (_writer, mut reader) = UnixStream::pair().expect("pair");
        configure_with_timeout(&reader, Duration::from_millis(20)).expect("configures");
        let error = read_json::<Value>(&mut reader).expect_err("must time out");
        assert!(matches!(error, Error::Io { .. }));
    }
}
