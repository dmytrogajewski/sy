//! Bounded connector for recipe-approved engine endpoints.

use std::{collections::BTreeSet, net::IpAddr, sync::Arc, time::Duration};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{client::conn::http1, header, Request};
use hyper_util::rt::TokioIo;

pub const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_ROUTE_COUNT: usize = 32;
const MAX_PATH_BYTES: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_SEMANTIC_PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONCURRENT_CONNECTIONS: usize = 8;
const STREAM_CHANNEL_CAPACITY: usize = 8;
const MAX_STREAM_EVENT_BYTES: usize = 64 * 1024;
const MAX_SEMANTIC_TEXT_BYTES: usize = 4096;
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const PROTOCOL_TOOL_MAX_TOKENS: u32 = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamError(&'static str);

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for UpstreamError {}

impl UpstreamError {
    pub const fn identity_mismatch() -> Self {
        Self("engine route identity changed")
    }

    pub const fn diagnostic(&self) -> &'static str {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRequest {
    method: String,
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamResponse {
    pub status: u16,
    pub bytes: Vec<u8>,
}

/// One validated local image. Public protocol shapes are discarded before an
/// engine request is constructed, so no URL or host path survives here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePart {
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct VisionProbe<'a> {
    pub served_model: &'a str,
    pub prompt: &'a str,
    pub image: &'a ImagePart,
    pub expected_text: &'a str,
    pub max_tokens: u32,
    pub disable_thinking: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingVector {
    pub index: usize,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingBatch {
    pub vectors: Vec<EmbeddingVector>,
    pub prompt_tokens: u64,
    pub total_tokens: u64,
}

pub fn decode_embedding_response(
    bytes: &[u8],
    served_model: &str,
    expected_count: usize,
    dimensions: usize,
    normalized: bool,
    tolerance_ppm: u32,
) -> Result<EmbeddingBatch, UpstreamError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(UpstreamError("engine embedding response is too large"));
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| UpstreamError("engine embedding response is invalid"))?;
    let data = value["data"]
        .as_array()
        .filter(|data| data.len() == expected_count)
        .ok_or(UpstreamError("engine embedding count changed"))?;
    if value["object"] != "list" || value["model"] != served_model {
        return Err(UpstreamError("engine embedding identity changed"));
    }
    let mut vectors = Vec::with_capacity(data.len());
    for (expected_index, item) in data.iter().enumerate() {
        let values = item["embedding"]
            .as_array()
            .filter(|values| values.len() == dimensions)
            .ok_or(UpstreamError("engine embedding dimension changed"))?
            .iter()
            .map(|value| {
                value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .map(|value| value as f32)
                    .filter(|value| value.is_finite())
                    .ok_or(UpstreamError(
                        "engine embedding contains a non-finite value",
                    ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if item["object"] != "embedding" || item["index"].as_u64() != Some(expected_index as u64) {
            return Err(UpstreamError("engine embedding order changed"));
        }
        let norm = values
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>()
            .sqrt();
        let tolerance = f64::from(tolerance_ppm) / 1_000_000.0;
        if normalized && (norm - 1.0).abs() > tolerance {
            return Err(UpstreamError("engine embedding normalization changed"));
        }
        vectors.push(EmbeddingVector {
            index: expected_index,
            values,
        });
    }
    let prompt_tokens = value["usage"]["prompt_tokens"]
        .as_u64()
        .ok_or(UpstreamError("engine embedding usage is invalid"))?;
    let total_tokens = value["usage"]["total_tokens"]
        .as_u64()
        .filter(|total| *total == prompt_tokens)
        .ok_or(UpstreamError("engine embedding usage is invalid"))?;
    Ok(EmbeddingBatch {
        vectors,
        prompt_tokens,
        total_tokens,
    })
}

/// Protocol-neutral generation events consumed independently by the OpenAI and
/// Anthropic encoders; public wire documents never enter this connector.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GenerationEvent {
    ReasoningDelta {
        text: String,
    },
    TextDelta {
        text: String,
    },
    ToolCallDelta {
        index: u32,
        call_id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    Finished {
        finish_reason: Option<String>,
    },
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    Done,
}

pub struct CompletionStream {
    receiver: tokio::sync::mpsc::Receiver<Result<GenerationEvent, UpstreamError>>,
    task: tokio::task::JoinHandle<()>,
    connection_task: tokio::task::JoinHandle<()>,
    idle_timeout: Duration,
}

impl CompletionStream {
    pub async fn next(&mut self) -> Option<Result<GenerationEvent, UpstreamError>> {
        match tokio::time::timeout(self.idle_timeout, self.receiver.recv()).await {
            Ok(event) => event,
            Err(_) => Some(Err(UpstreamError("engine stream idle timeout"))),
        }
    }
}

impl Drop for CompletionStream {
    fn drop(&mut self) {
        self.task.abort();
        self.connection_task.abort();
    }
}

pub struct RawResponseStream {
    receiver: tokio::sync::mpsc::Receiver<Result<Bytes, UpstreamError>>,
    task: tokio::task::JoinHandle<()>,
    connection_task: tokio::task::JoinHandle<()>,
}

impl RawResponseStream {
    pub async fn next(&mut self) -> Option<Result<Bytes, UpstreamError>> {
        match tokio::time::timeout(DEFAULT_STREAM_IDLE_TIMEOUT, self.receiver.recv()).await {
            Ok(chunk) => chunk,
            Err(_) => Some(Err(UpstreamError("engine stream idle timeout"))),
        }
    }
}

impl Drop for RawResponseStream {
    fn drop(&mut self) {
        self.task.abort();
        self.connection_task.abort();
    }
}

/// An endpoint proved by the executor from one managed container inspection.
///
/// The address and port are constructor-only and never accepted from an HTTP
/// request. Recreating an engine requires constructing a new generation-bound
/// route after another executor inspection.
#[derive(Debug, Clone)]
pub struct ObservedRoute {
    instance_id: String,
    generation: u64,
    address: IpAddr,
    port: u16,
    allowed: BTreeSet<(String, String)>,
    connections: Arc<tokio::sync::Semaphore>,
}

impl ObservedRoute {
    pub fn new<'a>(
        instance_id: &str,
        generation: u64,
        address: IpAddr,
        port: u16,
        allowed: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, UpstreamError> {
        if instance_id.is_empty() || instance_id.len() > 96 || generation == 0 || port == 0 {
            return Err(UpstreamError("invalid observed engine identity"));
        }
        let address_allowed = match address {
            IpAddr::V4(value) => value.is_private(),
            IpAddr::V6(value) => value.is_unique_local(),
        } || cfg!(test) && address.is_loopback();
        if !address_allowed {
            return Err(UpstreamError("engine address is outside a private bridge"));
        }
        let allowed = allowed
            .into_iter()
            .map(|(method, path)| (method.to_owned(), path.to_owned()))
            .collect::<BTreeSet<_>>();
        if allowed.is_empty()
            || allowed.len() > MAX_ROUTE_COUNT
            || allowed
                .iter()
                .any(|(method, path)| !valid_method(method) || !valid_path(path))
        {
            return Err(UpstreamError("recipe route allowlist is invalid"));
        }
        Ok(Self {
            instance_id: instance_id.into(),
            generation,
            address,
            port,
            allowed,
            connections: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNECTIONS)),
        })
    }

    pub fn request(
        &self,
        method: &str,
        path: &str,
        body_bytes: usize,
    ) -> Result<UpstreamRequest, UpstreamError> {
        if body_bytes > MAX_BODY_BYTES {
            return Err(UpstreamError("engine request body is too large"));
        }
        let key = (method.to_owned(), path.to_owned());
        if !self.allowed.contains(&key) {
            return Err(UpstreamError("engine method or route is not allowed"));
        }
        Ok(UpstreamRequest {
            method: key.0,
            path: key.1,
        })
    }

    pub async fn send(
        &self,
        request: &UpstreamRequest,
        body: &[u8],
    ) -> Result<UpstreamResponse, UpstreamError> {
        self.send_with_timeout(request, body, REQUEST_TIMEOUT).await
    }

    pub async fn send_with_timeout(
        &self,
        request: &UpstreamRequest,
        body: &[u8],
        timeout: Duration,
    ) -> Result<UpstreamResponse, UpstreamError> {
        self.request(&request.method, &request.path, body.len())?;
        tokio::time::timeout(timeout, self.send_inner(request, body))
            .await
            .map_err(|_| UpstreamError("engine request timed out"))?
    }

    pub async fn semantic_probe(
        &self,
        served_model: &str,
        prompt: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> Result<(), UpstreamError> {
        let models = self.request("GET", "/v1/models", 0)?;
        let models = self.send(&models, &[]).await?;
        let models_status = models.status;
        let models: serde_json::Value = serde_json::from_slice(&models.bytes)
            .map_err(|_| UpstreamError("engine model identity response is invalid"))?;
        let identity_matches = models["data"].as_array().is_some_and(|models| {
            models
                .iter()
                .any(|model| model["id"].as_str() == Some(served_model))
        });
        if !(200..300).contains(&models_status) || !identity_matches {
            return Err(UpstreamError("engine served-model identity mismatch"));
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "model": served_model,
            "prompt": prompt,
            "max_tokens": max_tokens,
            "temperature": 0,
            "ignore_eos": true,
            "stream": false
        }))
        .map_err(|_| UpstreamError("semantic probe cannot be encoded"))?;
        let completion = self.request("POST", "/v1/completions", body.len())?;
        let completion = self.send_with_timeout(&completion, &body, timeout).await?;
        let completion_status = completion.status;
        let completion: serde_json::Value = serde_json::from_slice(&completion.bytes)
            .map_err(|_| UpstreamError("semantic probe response is invalid"))?;
        if !(200..300).contains(&completion_status)
            || !valid_semantic_completion(&completion, served_model, max_tokens)
        {
            return Err(UpstreamError("semantic completion contract rejected"));
        }
        Ok(())
    }

    pub async fn embedding_probe(
        &self,
        served_model: &str,
        input: &str,
        dimensions: usize,
        normalized: bool,
        tolerance_ppm: u32,
        timeout: Duration,
    ) -> Result<(), UpstreamError> {
        let models = self.request("GET", "/v1/models", 0)?;
        let models = self.send(&models, &[]).await?;
        let identity: serde_json::Value = serde_json::from_slice(&models.bytes)
            .map_err(|_| UpstreamError("engine model identity response is invalid"))?;
        if !(200..300).contains(&models.status)
            || !identity["data"].as_array().is_some_and(|models| {
                models
                    .iter()
                    .any(|model| model["id"].as_str() == Some(served_model))
            })
        {
            return Err(UpstreamError("engine served-model identity mismatch"));
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "model":served_model,"input":input,"encoding_format":"float"
        }))
        .map_err(|_| UpstreamError("embedding probe cannot be encoded"))?;
        let request = self.request("POST", "/v1/embeddings", body.len())?;
        let response = self.send_with_timeout(&request, &body, timeout).await?;
        if !(200..300).contains(&response.status) {
            return Err(UpstreamError("embedding probe was rejected"));
        }
        decode_embedding_response(
            &response.bytes,
            served_model,
            1,
            dimensions,
            normalized,
            tolerance_ppm,
        )?;
        Ok(())
    }

    pub async fn vision_probe(
        &self,
        probe: VisionProbe<'_>,
        timeout: Duration,
    ) -> Result<(), UpstreamError> {
        let VisionProbe {
            served_model,
            prompt,
            image,
            expected_text,
            max_tokens,
            disable_thinking,
        } = probe;
        let models = self.request("GET", "/v1/models", 0)?;
        let models = self.send(&models, &[]).await?;
        let identity: serde_json::Value = serde_json::from_slice(&models.bytes)
            .map_err(|_| UpstreamError("engine model identity response is invalid"))?;
        if !(200..300).contains(&models.status)
            || !identity["data"].as_array().is_some_and(|models| {
                models
                    .iter()
                    .any(|model| model["id"].as_str() == Some(served_model))
            })
        {
            return Err(UpstreamError("engine served-model identity mismatch"));
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "model":served_model,
            "messages":[{"role":"user","content":[
                {"type":"text","text":prompt},
                {"type":"image_url","image_url":{"url":format!(
                    "data:{};base64,{}", image.media_type, BASE64.encode(&image.bytes)
                )}}
            ]}],
            "max_tokens":max_tokens,"thinking_budget_tokens":if disable_thinking { 0 } else { -1 },"temperature":0,"stream":false,
            "chat_template_kwargs":{"enable_thinking":!disable_thinking,"reasoning_strength":if disable_thinking { "low" } else { "high" }}
        }))
        .map_err(|_| UpstreamError("vision probe cannot be encoded"))?;
        let request = self.request("POST", "/v1/chat/completions", body.len())?;
        let response = self.send_with_timeout(&request, &body, timeout).await?;
        if !(200..300).contains(&response.status) {
            return Err(UpstreamError(match response.status {
                400..=499 => "vision probe rejected with HTTP 4xx",
                500..=599 => "vision probe rejected with HTTP 5xx",
                _ => "vision probe returned a non-success HTTP status",
            }));
        }
        let value: serde_json::Value = serde_json::from_slice(&response.bytes)
            .map_err(|_| UpstreamError("vision probe response is invalid"))?;
        if value["model"] != served_model {
            return Err(UpstreamError("vision probe model identity changed"));
        }
        let choice = value["choices"]
            .as_array()
            .filter(|choices| choices.len() == 1)
            .map(|choices| &choices[0])
            .ok_or(UpstreamError("vision probe choice shape is invalid"))?;
        let content = choice["message"]["content"]
            .as_str()
            .ok_or(UpstreamError("vision probe final content is missing"))?;
        if !content
            .to_lowercase()
            .contains(&expected_text.to_lowercase())
        {
            return Err(UpstreamError("vision probe expected answer is missing"));
        }
        let prompt_tokens = value["usage"]["prompt_tokens"].as_u64();
        let completion_tokens = value["usage"]["completion_tokens"].as_u64();
        if prompt_tokens.is_none_or(|tokens| tokens == 0)
            || completion_tokens.is_none_or(|tokens| tokens == 0 || tokens > u64::from(max_tokens))
        {
            return Err(UpstreamError("vision probe usage contract is invalid"));
        }
        Ok(())
    }

    pub async fn protocol_probe(
        &self,
        served_model: &str,
        require_tools: bool,
        timeout: Duration,
    ) -> Result<(), UpstreamError> {
        tokio::time::timeout(
            timeout,
            self.protocol_probe_inner(served_model, require_tools),
        )
        .await
        .map_err(|_| UpstreamError("engine protocol probe timed out"))?
    }

    async fn protocol_probe_inner(
        &self,
        served_model: &str,
        require_tools: bool,
    ) -> Result<(), UpstreamError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "model":served_model,"messages":[{"role":"user","content":"Reply with OK."}],
            "max_tokens":PROTOCOL_TOOL_MAX_TOKENS,"temperature":0,"stream":true,
            "stream_options":{"include_usage":true},"chat_template_kwargs":{"enable_thinking":false,"reasoning_strength":"low"}
        }))
        .map_err(|_| UpstreamError("protocol probe cannot be encoded"))?;
        validate_protocol_stream(self.chat_stream(&body).await?, false).await?;
        if require_tools {
            let body = serde_json::to_vec(&serde_json::json!({
                "model":served_model,
                "messages":[{"role":"user","content":"Call capability_probe with value ok."}],
                "tools":[{"type":"function","function":{"name":"capability_probe",
                    "description":"Validate one tool call.","parameters":{"type":"object",
                    "properties":{"value":{"type":"string"}},"required":["value"]}}}],
                "tool_choice":"required",
                "parallel_tool_calls":true,"max_tokens":PROTOCOL_TOOL_MAX_TOKENS,
                "temperature":0,"stream":true,"stream_options":{"include_usage":true},"chat_template_kwargs":{"enable_thinking":false,"reasoning_strength":"low"}
            }))
            .map_err(|_| UpstreamError("tool protocol probe cannot be encoded"))?;
            validate_protocol_stream(self.chat_stream(&body).await?, true).await?;
        }
        Ok(())
    }

    pub async fn completion_stream(&self, body: &[u8]) -> Result<CompletionStream, UpstreamError> {
        self.generation_stream("/v1/completions", body, DEFAULT_STREAM_IDLE_TIMEOUT)
            .await
    }

    pub async fn chat_stream(&self, body: &[u8]) -> Result<CompletionStream, UpstreamError> {
        self.chat_stream_with_idle_timeout(body, DEFAULT_STREAM_IDLE_TIMEOUT)
            .await
    }

    pub async fn chat_stream_with_idle_timeout(
        &self,
        body: &[u8],
        idle_timeout: Duration,
    ) -> Result<CompletionStream, UpstreamError> {
        self.generation_stream("/v1/chat/completions", body, idle_timeout)
            .await
    }

    pub async fn raw_stream(
        &self,
        path: &str,
        body: &[u8],
    ) -> Result<RawResponseStream, UpstreamError> {
        let request = self.request("POST", path, body.len())?;
        let permit = Arc::clone(&self.connections)
            .acquire_owned()
            .await
            .map_err(|_| UpstreamError("engine connection pool is closed"))?;
        let stream = tokio::net::TcpStream::connect((self.address, self.port))
            .await
            .map_err(|_| UpstreamError("engine endpoint is unreachable"))?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| UpstreamError("engine HTTP handshake failed"))?;
        let connection_task = tokio::spawn(async move {
            let _ = connection.await;
        });
        let outbound = Request::builder()
            .method(request.method.as_str())
            .uri(request.path.as_str())
            .header(header::HOST, "engine.internal")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::copy_from_slice(body)))
            .map_err(|_| UpstreamError("engine request is malformed"))?;
        let response = sender
            .send_request(outbound)
            .await
            .map_err(|_| UpstreamError("engine response failed"))?;
        if !(200..300).contains(&response.status().as_u16()) {
            connection_task.abort();
            return Err(UpstreamError("engine rejected response stream"));
        }
        let (send, receiver) = tokio::sync::mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let mut incoming = response.into_body();
        let task = tokio::spawn(async move {
            let _permit = permit;
            while let Some(frame) = incoming.frame().await {
                let Ok(frame) = frame else {
                    let _ = send.send(Err(UpstreamError("engine stream failed"))).await;
                    break;
                };
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                for chunk in data.chunks(MAX_STREAM_EVENT_BYTES) {
                    if send.send(Ok(Bytes::copy_from_slice(chunk))).await.is_err() {
                        return;
                    }
                }
            }
        });
        Ok(RawResponseStream {
            receiver,
            task,
            connection_task,
        })
    }

    async fn generation_stream(
        &self,
        path: &str,
        body: &[u8],
        idle_timeout: Duration,
    ) -> Result<CompletionStream, UpstreamError> {
        let request = self.request("POST", path, body.len())?;
        let permit = Arc::clone(&self.connections)
            .acquire_owned()
            .await
            .map_err(|_| UpstreamError("engine connection pool is closed"))?;
        let stream = tokio::net::TcpStream::connect((self.address, self.port))
            .await
            .map_err(|_| UpstreamError("engine endpoint is unreachable"))?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| UpstreamError("engine HTTP handshake failed"))?;
        let connection_task = tokio::spawn(async move {
            let _ = connection.await;
        });
        let outbound = Request::builder()
            .method(request.method.as_str())
            .uri(request.path.as_str())
            .header(header::HOST, "engine.internal")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::copy_from_slice(body)))
            .map_err(|_| UpstreamError("engine request is malformed"))?;
        let response = sender
            .send_request(outbound)
            .await
            .map_err(|_| UpstreamError("engine response failed"))?;
        if !(200..300).contains(&response.status().as_u16()) {
            return Err(UpstreamError("engine rejected completion stream"));
        }
        let (send, receiver) = tokio::sync::mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let mut incoming = response.into_body();
        let task = tokio::spawn(async move {
            let _permit = permit;
            let mut pending = Vec::new();
            while let Some(frame) = incoming.frame().await {
                let Ok(frame) = frame else { break };
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                pending.extend_from_slice(&data);
                if pending.len() > MAX_STREAM_EVENT_BYTES {
                    let _ = send
                        .send(Err(UpstreamError("engine stream event is too large")))
                        .await;
                    break;
                }
                while let Some(end) = pending.windows(2).position(|bytes| bytes == b"\n\n") {
                    let event = pending.drain(..end + 2).collect::<Vec<_>>();
                    for decoded in decode_sse_events(&event) {
                        let done = matches!(decoded, Ok(GenerationEvent::Done));
                        if send.send(decoded).await.is_err() || done {
                            return;
                        }
                    }
                }
            }
        });
        Ok(CompletionStream {
            receiver,
            task,
            connection_task,
            idle_timeout,
        })
    }

    async fn send_inner(
        &self,
        request: &UpstreamRequest,
        body: &[u8],
    ) -> Result<UpstreamResponse, UpstreamError> {
        let _permit = self
            .connections
            .acquire()
            .await
            .map_err(|_| UpstreamError("engine connection pool is closed"))?;
        let stream = tokio::net::TcpStream::connect((self.address, self.port))
            .await
            .map_err(|_| UpstreamError("engine endpoint is unreachable"))?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| UpstreamError("engine HTTP handshake failed"))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let outbound = Request::builder()
            .method(request.method.as_str())
            .uri(request.path.as_str())
            .header(header::HOST, "engine.internal")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::copy_from_slice(body)))
            .map_err(|_| UpstreamError("engine request is malformed"))?;
        let response = sender
            .send_request(outbound)
            .await
            .map_err(|_| UpstreamError("engine response failed"))?;
        let status = response.status().as_u16();
        let bytes = Limited::new(response.into_body(), MAX_RESPONSE_BYTES)
            .collect()
            .await
            .map_err(|_| UpstreamError("engine response is too large"))?
            .to_bytes()
            .to_vec();
        Ok(UpstreamResponse { status, bytes })
    }

    pub fn identity(&self) -> (&str, u64) {
        (&self.instance_id, self.generation)
    }
}

async fn validate_protocol_stream(
    mut stream: CompletionStream,
    require_tool: bool,
) -> Result<(), UpstreamError> {
    let mut output = false;
    let mut tools = std::collections::BTreeMap::<u32, (String, String, String)>::new();
    let mut finish_reason = None;
    let mut usage = false;
    while let Some(event) = stream.next().await {
        match event? {
            GenerationEvent::ReasoningDelta { text } | GenerationEvent::TextDelta { text } => {
                if finish_reason.is_some() || text.is_empty() {
                    return Err(UpstreamError("engine protocol event order is invalid"));
                }
                output = true;
            }
            GenerationEvent::ToolCallDelta {
                index,
                call_id,
                name,
                arguments,
            } => {
                if finish_reason.is_some() || !require_tool {
                    return Err(UpstreamError("engine protocol tool event is invalid"));
                }
                let tool = tools.entry(index).or_default();
                if let Some(call_id) = call_id {
                    tool.0 = call_id;
                }
                if let Some(name) = name {
                    tool.1 = name;
                }
                tool.2.push_str(&arguments);
            }
            GenerationEvent::Finished {
                finish_reason: reason,
            } => {
                if finish_reason.is_some() || reason.is_none() {
                    return Err(UpstreamError("engine protocol finish event is invalid"));
                }
                finish_reason = reason;
            }
            GenerationEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                if finish_reason.is_none() || usage || prompt_tokens == 0 || completion_tokens == 0
                {
                    return Err(UpstreamError("engine protocol usage event is invalid"));
                }
                usage = true;
            }
            GenerationEvent::Done => {
                let tools_valid = !tools.is_empty()
                    && tools.values().all(|(call_id, name, arguments)| {
                        !call_id.is_empty()
                            && name == "capability_probe"
                            && serde_json::from_str::<serde_json::Value>(arguments)
                                .is_ok_and(|value| value["value"] == "ok")
                    });
                let finish_valid = if require_tool {
                    finish_reason.as_deref() == Some("tool_calls")
                } else {
                    matches!(finish_reason.as_deref(), Some("stop" | "length"))
                };
                if finish_valid && usage && (require_tool && tools_valid || !require_tool && output)
                {
                    return Ok(());
                }
                return Err(UpstreamError("engine protocol stream is incomplete"));
            }
        }
    }
    Err(UpstreamError("engine protocol stream ended before done"))
}

fn valid_semantic_completion(
    completion: &serde_json::Value,
    served_model: &str,
    max_tokens: u32,
) -> bool {
    let Some(id) = completion["id"].as_str() else {
        return false;
    };
    let Some(choices) = completion["choices"].as_array() else {
        return false;
    };
    let [choice] = choices.as_slice() else {
        return false;
    };
    let Some(text) = choice["text"].as_str() else {
        return false;
    };
    completion["object"].as_str() == Some("text_completion")
        && completion["model"].as_str() == Some(served_model)
        && !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && choice["index"].as_u64() == Some(0)
        && matches!(choice["finish_reason"].as_str(), Some("length" | "stop"))
        && text.len() <= MAX_SEMANTIC_TEXT_BYTES
        && completion["usage"]["prompt_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0)
        && completion["usage"]["completion_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0 && tokens <= u64::from(max_tokens))
        && max_tokens > 0
}

fn decode_sse_events(bytes: &[u8]) -> Vec<Result<GenerationEvent, UpstreamError>> {
    let data = std::str::from_utf8(bytes)
        .ok()
        .into_iter()
        .flat_map(str::lines)
        .find_map(|line| line.strip_prefix("data: "));
    let Some(data) = data else {
        return Vec::new();
    };
    if data == "[DONE]" {
        return vec![Ok(GenerationEvent::Done)];
    }
    let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(_) => return vec![Err(UpstreamError("engine stream event is invalid"))],
    };
    if value.get("error").is_some() {
        return vec![Err(UpstreamError("engine stream returned an error"))];
    }
    let mut events = Vec::new();
    let choice = value["choices"]
        .as_array()
        .and_then(|choices| choices.first());
    if let Some(choice) = choice {
        let delta = &choice["delta"];
        if let Some(text) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
            .filter(|text| !text.is_empty())
        {
            events.push(Ok(GenerationEvent::ReasoningDelta { text: text.into() }));
        }
        if let Some(text) = choice["text"]
            .as_str()
            .or_else(|| delta["content"].as_str())
            .filter(|text| !text.is_empty())
        {
            events.push(Ok(GenerationEvent::TextDelta { text: text.into() }));
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let Some(index) = call["index"]
                    .as_u64()
                    .and_then(|value| value.try_into().ok())
                else {
                    return vec![Err(UpstreamError("engine tool call index is invalid"))];
                };
                events.push(Ok(GenerationEvent::ToolCallDelta {
                    index,
                    call_id: call["id"].as_str().map(str::to_owned),
                    name: call["function"]["name"].as_str().map(str::to_owned),
                    arguments: call["function"]["arguments"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                }));
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            events.push(Ok(GenerationEvent::Finished {
                finish_reason: Some(reason.into()),
            }));
        }
    }
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        let Some(prompt_tokens) = usage["prompt_tokens"].as_u64() else {
            return vec![Err(UpstreamError("engine stream usage is invalid"))];
        };
        let Some(completion_tokens) = usage["completion_tokens"].as_u64() else {
            return vec![Err(UpstreamError("engine stream usage is invalid"))];
        };
        events.push(Ok(GenerationEvent::Usage {
            prompt_tokens,
            completion_tokens,
        }));
    }
    events
}

#[cfg(test)]
fn decode_sse_event(bytes: &[u8]) -> Option<Result<GenerationEvent, UpstreamError>> {
    decode_sse_events(bytes).into_iter().next()
}

fn valid_method(value: &str) -> bool {
    matches!(value, "GET" | "POST")
}

fn valid_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PATH_BYTES
        && value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains("..")
        && !value.contains('?')
        && !value.contains('#')
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn fake_vllm() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let count = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..count]);
                let body = if request.starts_with("GET /v1/models ") {
                    r#"{"object":"list","data":[{"id":"Ornith-1.5-9B"}]}"#.to_owned()
                } else if request.contains("\"stream\":true") {
                    "data: {\"choices\":[{\"text\":\"OK\",\"finish_reason\":null}]}\n\ndata: [DONE]\n\n".into()
                } else {
                    r#"{"id":"cmpl-semantic","object":"text_completion","model":"Ornith-1.5-9B","choices":[{"index":0,"text":"","finish_reason":"length"}],"usage":{"prompt_tokens":5,"completion_tokens":1}}"#.to_owned()
                };
                let content_type = if body.starts_with("data:") {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (address, task)
    }

    async fn fake_llama_protocol() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for tool_probe in [false, true] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8192];
                let count = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..count]);
                assert!(request.contains(r#""stream_options":{"include_usage":true}"#));
                assert!(request.contains(r#""max_tokens":256"#));
                assert!(request.contains(r#""reasoning_strength":"low""#));
                assert_eq!(request.contains("capability_probe"), tool_probe);
                assert_eq!(request.contains(r#""tool_choice":"required""#), tool_probe);
                let deltas = if tool_probe {
                    [
                        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"capability_probe","arguments":"{\"value\":\""}}]},"finish_reason":null}]}"#,
                        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ok\"}"}}]},"finish_reason":null}]}"#,
                        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                    ]
                } else {
                    [
                        r#"{"choices":[{"delta":{"reasoning_content":"inspect"},"finish_reason":null}]}"#,
                        r#"{"choices":[{"delta":{"content":"ready"},"finish_reason":null}]}"#,
                        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                    ]
                };
                let body = deltas.into_iter().map(|delta| format!("data: {delta}\n\n")).collect::<String>()
                    + "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n";
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        (address, task)
    }

    #[test]
    fn connector_cannot_target_caller_url_or_forbidden_route() {
        let route = ObservedRoute::new(
            "engine-1",
            7,
            "172.30.0.2".parse().unwrap(),
            8000,
            [("GET", "/health"), ("POST", "/v1/chat/completions")],
        )
        .unwrap();

        assert!(route.request("GET", "/health", 0).is_ok());
        assert!(route
            .request("POST", "http://attacker.invalid/", 0)
            .is_err());
        assert!(route.request("DELETE", "/health", 0).is_err());
    }

    #[test]
    fn request_bounds_are_enforced_before_network_io() {
        let route = ObservedRoute::new(
            "engine-1",
            7,
            "172.30.0.2".parse().unwrap(),
            8000,
            [("POST", "/probe")],
        )
        .unwrap();

        assert!(route.request("POST", "/probe", MAX_BODY_BYTES + 1).is_err());
    }

    #[test]
    fn vllm_completion_chunks_decode_to_protocol_neutral_events() {
        let event = decode_sse_event(
            b"data: {\"choices\":[{\"text\":\"hello\",\"finish_reason\":null}]}\n\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            event,
            GenerationEvent::TextDelta {
                text: "hello".into()
            }
        );
    }

    #[test]
    fn llama_cpp_completion_final_chunk_preserves_finish_and_usage() {
        let events = decode_sse_events(
            b"data: {\"choices\":[{\"index\":0,\"text\":\"\",\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n",
        );
        assert_eq!(
            events,
            vec![
                Ok(GenerationEvent::Finished {
                    finish_reason: Some("stop".into())
                }),
                Ok(GenerationEvent::Usage {
                    prompt_tokens: 4,
                    completion_tokens: 2
                }),
            ]
        );
    }

    #[test]
    fn null_stream_usage_is_a_placeholder() {
        assert_eq!(
            decode_sse_events(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}],\"usage\":null}\n\n"
            ),
            vec![Ok(GenerationEvent::TextDelta { text: "ok".into() })]
        );
    }

    #[test]
    fn vllm_chat_tool_chunks_decode_to_protocol_neutral_events() {
        let event = decode_sse_event(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\"}}]}}]}\n\n")
            .unwrap().unwrap();
        assert_eq!(
            event,
            GenerationEvent::ToolCallDelta {
                index: 0,
                call_id: Some("call_1".into()),
                name: Some("shell".into()),
                arguments: "{\"cmd\":".into()
            }
        );
    }

    #[test]
    fn vllm_reasoning_chunks_decode_to_protocol_neutral_events() {
        for field in ["reasoning_content", "reasoning"] {
            let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"{field}\":\"inspect state\"}}}}]}}\n\n"
            );
            let event = decode_sse_event(body.as_bytes()).unwrap().unwrap();
            assert_eq!(
                event,
                GenerationEvent::ReasoningDelta {
                    text: "inspect state".into()
                }
            );
        }
    }

    #[test]
    fn llama_cpp_stream_maps_reasoning_text_tools_usage_and_done() {
        let chunk = b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"inspect\",\"content\":\"ready\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\"}},{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"patch\",\"arguments\":\"[\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n";
        assert_eq!(
            decode_sse_events(chunk),
            vec![
                Ok(GenerationEvent::ReasoningDelta {
                    text: "inspect".into()
                }),
                Ok(GenerationEvent::TextDelta {
                    text: "ready".into()
                }),
                Ok(GenerationEvent::ToolCallDelta {
                    index: 0,
                    call_id: Some("call_1".into()),
                    name: Some("lookup".into()),
                    arguments: "{".into()
                }),
                Ok(GenerationEvent::ToolCallDelta {
                    index: 1,
                    call_id: Some("call_2".into()),
                    name: Some("patch".into()),
                    arguments: "[".into()
                }),
                Ok(GenerationEvent::Finished {
                    finish_reason: Some("tool_calls".into())
                }),
                Ok(GenerationEvent::Usage {
                    prompt_tokens: 7,
                    completion_tokens: 3
                }),
            ]
        );
        assert_eq!(
            decode_sse_events(b"data: [DONE]\n\n"),
            vec![Ok(GenerationEvent::Done)]
        );
    }

    #[test]
    fn llama_cpp_stream_error_is_not_mistaken_for_clean_eof() {
        let event = decode_sse_events(
            b"data: {\"error\":{\"type\":\"server_error\",\"message\":\"private\"}}\n\n",
        );
        assert_eq!(
            event[0].as_ref().unwrap_err().diagnostic(),
            "engine stream returned an error"
        );
    }

    #[test]
    fn computed_token_allows_empty_or_special_rendered_text() {
        for text in ["", "\u{0}"] {
            let completion = serde_json::json!({
                "id": "cmpl-semantic",
                "object": "text_completion",
                "model": "Ornith-1.5-9B",
                "choices": [{"index": 0, "text": text, "finish_reason": "length"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 1},
            });
            assert!(valid_semantic_completion(&completion, "Ornith-1.5-9B", 1));
        }
    }

    #[test]
    fn semantic_contract_treats_max_tokens_as_an_upper_bound() {
        let completion = serde_json::json!({
            "id": "cmpl-semantic",
            "object": "text_completion",
            "model": "served",
            "choices": [{"index": 0, "text": "thinking", "finish_reason": "length"}],
            "usage": {"prompt_tokens": 8, "completion_tokens": 7},
        });

        assert!(valid_semantic_completion(&completion, "served", 8));
    }

    #[test]
    fn llama_cpp_completion_accepts_its_chatcmpl_identifier() {
        let completion = serde_json::json!({"id":"chatcmpl-semantic","object":"text_completion",
            "model":"served","choices":[{"index":0,"text":"ok","finish_reason":"length"}],
            "usage":{"prompt_tokens":5,"completion_tokens":1}});
        assert!(valid_semantic_completion(&completion, "served", 1));
    }

    #[test]
    fn semantic_contract_accepts_safe_opaque_identifier() {
        let completion = serde_json::json!({"id":"6be609c7b4064f3c8c3bdfb1d27d4521","object":"text_completion",
            "model":"served","choices":[{"index":0,"text":"ok","finish_reason":"length"}],
            "usage":{"prompt_tokens":5,"completion_tokens":1}});
        assert!(valid_semantic_completion(&completion, "served", 1));
    }

    #[test]
    fn semantic_contract_rejects_zero_token_and_spoofed_completion_shapes() {
        let choice = serde_json::json!({"index": 0, "text": "OK", "finish_reason": "length"});
        let invalid = [
            serde_json::json!({"error": {"message": "not a completion"}}),
            serde_json::json!({"id":"cmpl-semantic","object":"text_completion","model":"wrong","choices":[choice.clone()],"usage":{"prompt_tokens":5,"completion_tokens":1}}),
            serde_json::json!({"id":"cmpl-semantic","object":"text_completion","model":"Ornith-1.5-9B","choices":[choice.clone(), choice.clone()],"usage":{"prompt_tokens":5,"completion_tokens":1}}),
            serde_json::json!({"id":"cmpl-semantic","object":"text_completion","model":"Ornith-1.5-9B","choices":[choice.clone()],"usage":{"prompt_tokens":0,"completion_tokens":1}}),
            serde_json::json!({"id":"cmpl-semantic","object":"text_completion","model":"Ornith-1.5-9B","choices":[choice.clone()],"usage":{"prompt_tokens":5,"completion_tokens":0}}),
            serde_json::json!({"id":"cmpl-semantic","object":"text_completion","model":"Ornith-1.5-9B","choices":[choice.clone()],"usage":{"prompt_tokens":5,"completion_tokens":2}}),
            serde_json::json!({"id":"unsafe id","object":"text_completion","model":"Ornith-1.5-9B","choices":[choice],"usage":{"prompt_tokens":5,"completion_tokens":1}}),
        ];

        assert!(invalid.iter().all(|completion| !valid_semantic_completion(
            completion,
            "Ornith-1.5-9B",
            1
        )));
    }

    #[tokio::test]
    async fn slow_stream_is_bounded_and_client_disconnect_cancels_upstream() {
        struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(signal) = self.0.take() {
                    let _ = signal.send(());
                }
            }
        }

        let (send, receiver) = tokio::sync::mpsc::channel(STREAM_CHANNEL_CAPACITY);
        assert_eq!(send.capacity(), STREAM_CHANNEL_CAPACITY);
        let (dropped, cancelled) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _signal = DropSignal(Some(dropped));
            loop {
                if send
                    .send(Ok(GenerationEvent::TextDelta { text: "x".into() }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        let connection_task = tokio::spawn(std::future::pending());
        let stream = CompletionStream {
            receiver,
            task,
            connection_task,
            idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
        };
        tokio::task::yield_now().await;
        drop(stream);
        assert!(tokio::time::timeout(Duration::from_secs(1), cancelled)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn completion_stream_uses_its_configured_idle_timeout() {
        let (send, receiver) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = send.send(Ok(GenerationEvent::Done)).await;
        });
        let connection_task = tokio::spawn(std::future::pending());
        let mut stream = CompletionStream {
            receiver,
            task,
            connection_task,
            idle_timeout: Duration::from_millis(10),
        };
        assert_eq!(
            stream.next().await.unwrap().unwrap_err().diagnostic(),
            "engine stream idle timeout"
        );
    }

    #[tokio::test]
    async fn exact_vllm_identity_probe_and_completion_stream_use_real_http_wire() {
        let (address, server) = fake_vllm().await;
        let route = ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [("GET", "/v1/models"), ("POST", "/v1/completions")],
        )
        .unwrap();
        route
            .semantic_probe(
                "Ornith-1.5-9B",
                "Generate one completion token.",
                1,
                MAX_SEMANTIC_PROBE_TIMEOUT,
            )
            .await
            .unwrap();
        let mut stream = route
            .completion_stream(
                br#"{"model":"Ornith-1.5-9B","prompt":"OK","max_tokens":1,"stream":true}"#,
            )
            .await
            .unwrap();
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            GenerationEvent::TextDelta { text: "OK".into() }
        );
        drop(stream);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_response_stream_preserves_engine_sse_bytes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let count = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.starts_with("POST /v1/responses "));
            let body = "data: {\"type\":\"response.completed\"}\n\n";
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let route = ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [("POST", "/v1/responses")],
        )
        .unwrap();
        let mut stream = route
            .raw_stream("/v1/responses", br#"{"model":"public","stream":true}"#)
            .await
            .unwrap();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.unwrap());
        }

        assert_eq!(body, b"data: {\"type\":\"response.completed\"}\n\n");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn llama_cpp_protocol_probe_accepts_complete_streamed_chat_and_parallel_tools() {
        let (address, server) = fake_llama_protocol().await;
        let route = ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [("POST", "/v1/chat/completions")],
        )
        .unwrap();
        route
            .protocol_probe("fixture-model", true, Duration::from_secs(1))
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cold_first_token_within_semantic_deadline_succeeds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (arrived, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut arrived = Some(arrived);
            let mut released = Some(released);
            for request_index in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let _ = socket.read(&mut bytes).await.unwrap();
                let body = if request_index == 0 {
                    r#"{"data":[{"id":"Ornith-1.5-9B"}]}"#
                } else {
                    if let Some(sender) = arrived.take() {
                        let _ = sender.send(());
                    }
                    if let Some(receiver) = released.take() {
                        let _ = receiver.await;
                    }
                    r#"{"id":"cmpl-cold","object":"text_completion","model":"Ornith-1.5-9B","choices":[{"index":0,"text":"\u0000","finish_reason":"length"}],"usage":{"prompt_tokens":5,"completion_tokens":1}}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let route = ObservedRoute::new(
            "i_11111111111111111111111111111111",
            1,
            address.ip(),
            address.port(),
            [("GET", "/v1/models"), ("POST", "/v1/completions")],
        )
        .unwrap();
        let probe = tokio::spawn(async move {
            route
                .semantic_probe(
                    "Ornith-1.5-9B",
                    "Generate one completion token.",
                    1,
                    Duration::from_secs(120),
                )
                .await
        });
        waiting.await.unwrap();
        assert!(!probe.is_finished());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), probe)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }
}
