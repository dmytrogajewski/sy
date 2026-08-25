//! Strict, generation-safe inference gateway routing.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

use arc_swap::ArcSwap;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use image::{ImageFormat, ImageReader};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::upstream::{decode_embedding_response, GenerationEvent, ImagePart, ObservedRoute};

pub const MAX_COMPLETION_BODY_BYTES: usize = 1024 * 1024;
pub const RETRY_AFTER_SECONDS: &str = "1";
pub const MAX_OUTPUT_TOKENS: u64 = 32_768;
const MAX_INSTANCE_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisionPolicy {
    pub processor_sha256: String,
    pub media_types: BTreeSet<String>,
    pub max_bytes: usize,
    pub max_total_bytes: usize,
    pub max_count: usize,
    pub max_width: u32,
    pub max_height: u32,
    pub health_media_type: String,
    pub health_image_base64: String,
    pub health_image_sha256: String,
    pub health_prompt: String,
    pub health_expected_text: String,
    pub health_max_tokens: u32,
    pub health_disable_thinking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingPolicy {
    pub dimensions: usize,
    pub max_batch: usize,
    pub max_input_bytes: usize,
    pub normalized: bool,
    pub normalization_tolerance_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayProfile {
    pub capabilities: BTreeSet<String>,
    pub vision: Option<VisionPolicy>,
    pub embeddings: Option<EmbeddingPolicy>,
}

impl GatewayProfile {
    pub fn text() -> Self {
        Self {
            capabilities: ["text_generation".into(), "tool_calling".into()].into(),
            vision: None,
            embeddings: None,
        }
    }

    #[cfg(test)]
    pub fn embedding(
        dimensions: usize,
        max_batch: usize,
        max_input_bytes: usize,
        normalized: bool,
        normalization_tolerance_ppm: u32,
    ) -> Self {
        Self {
            capabilities: ["text_embeddings".into()].into(),
            vision: None,
            embeddings: Some(EmbeddingPolicy {
                dimensions,
                max_batch,
                max_input_bytes,
                normalized,
                normalization_tolerance_ppm,
            }),
        }
    }

    pub fn allows(&self, action: PublicAction) -> bool {
        match action {
            PublicAction::Models => !self.capabilities.is_empty(),
            PublicAction::Completions | PublicAction::Chat | PublicAction::Responses => {
                self.capabilities.contains("text_generation")
            }
            PublicAction::Embeddings => self.capabilities.contains("text_embeddings"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GenerationRequest {
    pub body: Vec<u8>,
    pub stream: bool,
    pub custom_tools: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct EmbeddingsRequest {
    pub body: Vec<u8>,
    pub input_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiError {
    pub code: &'static str,
    pub message: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiStreamEvent {
    pub name: &'static str,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicStreamEvent {
    pub name: &'static str,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicError {
    pub error_type: &'static str,
    pub message: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicMessageRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(default)]
    system: Option<AnthropicContent>,
    max_tokens: u64,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    tools: Vec<AnthropicTool>,
    #[serde(default)]
    tool_choice: Option<AnthropicToolChoice>,
    #[serde(default)]
    stop_sequences: Vec<String>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    top_k: Option<u64>,
    #[serde(default)]
    metadata: Option<AnthropicMetadata>,
    #[serde(default)]
    thinking: Option<AnthropicThinking>,
    #[serde(default)]
    context_management: Option<AnthropicContextManagement>,
    #[serde(default)]
    output_config: Option<AnthropicOutputConfig>,
    #[serde(default)]
    service_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicMessage {
    role: String,
    content: AnthropicContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum AnthropicContentBlock {
    Text {
        text: String,
        #[serde(default)]
        cache_control: Option<AnthropicCacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default)]
        cache_control: Option<AnthropicCacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: AnthropicToolResultContent,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        cache_control: Option<AnthropicCacheControl>,
    },
    Image {
        source: AnthropicImageSource,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    kind: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AnthropicToolResultContent {
    Text(String),
    Blocks(Vec<AnthropicToolResultText>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicToolResultText {
    #[serde(rename = "type")]
    kind: String,
    text: String,
    #[serde(default)]
    cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicCacheControl {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    ttl: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicTool {
    name: String,
    #[serde(default)]
    description: String,
    input_schema: Value,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicToolChoice {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    disable_parallel_tool_use: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicMetadata {
    #[serde(default)]
    user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    budget_tokens: Option<u64>,
    #[serde(default)]
    display: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicContextManagement {
    #[serde(default)]
    edits: Vec<AnthropicContextEdit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicContextEdit {
    #[serde(rename = "type")]
    kind: String,
    keep: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicOutputConfig {
    effort: String,
}

#[derive(Debug, Default)]
struct ToolCall {
    call_id: String,
    name: String,
    arguments: String,
    announced: bool,
}

#[derive(Debug, Default)]
struct AnthropicToolCall {
    call_id: String,
    name: String,
    arguments: String,
    block_index: Option<u32>,
}

pub struct AnthropicEncoder {
    id: String,
    model: String,
    text: String,
    text_index: Option<u32>,
    tools: BTreeMap<u32, AnthropicToolCall>,
    usage: (u64, u64),
    output_bytes: usize,
    finish_reason: Option<String>,
    next_index: u32,
    pending: VecDeque<AnthropicStreamEvent>,
}

impl AnthropicEncoder {
    pub fn new(model: String) -> Self {
        let id = format!("msg_{}", ulid::Ulid::new().to_string().to_ascii_lowercase());
        let mut encoder = Self {
            id,
            model,
            text: String::new(),
            text_index: None,
            tools: BTreeMap::new(),
            usage: (0, 0),
            output_bytes: 0,
            finish_reason: None,
            next_index: 0,
            pending: VecDeque::new(),
        };
        encoder.event(
            "message_start",
            serde_json::json!({"message":{
            "id":encoder.id,"type":"message","role":"assistant","content":[],
            "model":encoder.model,"stop_reason":null,"stop_sequence":null,
            "usage":{"input_tokens":0,"output_tokens":0}}}),
        );
        encoder
    }

    pub fn pop(&mut self) -> Option<AnthropicStreamEvent> {
        self.pending.pop_front()
    }

    pub fn final_document(&self) -> Value {
        let mut content = Vec::new();
        if self.text_index.is_some() {
            content.push(serde_json::json!({"type":"text","text":self.text}));
        }
        content.extend(self.tools.values().map(|tool| serde_json::json!({
            "type":"tool_use","id":tool.call_id,"name":tool.name,
            "input":serde_json::from_str::<Value>(&tool.arguments).unwrap_or_else(|_| serde_json::json!({}))
        })));
        serde_json::json!({"id":self.id,"type":"message","role":"assistant",
            "model":self.model,"content":content,"stop_reason":anthropic_stop_reason(self.finish_reason.as_deref()),
            "stop_sequence":null,"usage":{"input_tokens":self.usage.0,"output_tokens":self.usage.1}})
    }

    pub fn accept(&mut self, event: GenerationEvent) -> Result<(), AnthropicError> {
        match event {
            GenerationEvent::TextDelta { text } => self.text_delta(text),
            GenerationEvent::ToolCallDelta {
                index,
                call_id,
                name,
                arguments,
            } => self.tool_delta(index, call_id, name, arguments),
            GenerationEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.usage = (prompt_tokens, completion_tokens);
                Ok(())
            }
            GenerationEvent::Finished { finish_reason } => {
                self.finish_reason = finish_reason;
                Ok(())
            }
            GenerationEvent::Done => self.done(),
        }
    }

    pub fn fail(&mut self) {
        self.event(
            "error",
            serde_json::json!({"error":{
            "type":"api_error","message":"upstream stream failed"}}),
        );
    }

    fn event(&mut self, name: &'static str, fields: Value) {
        let mut data = fields.as_object().cloned().unwrap_or_default();
        data.insert("type".into(), Value::String(name.into()));
        self.pending.push_back(AnthropicStreamEvent {
            name,
            data: Value::Object(data),
        });
    }

    fn text_delta(&mut self, text: String) -> Result<(), AnthropicError> {
        if self.output_bytes.saturating_add(text.len()) > MAX_COMPLETION_BODY_BYTES {
            return Err(anthropic_invalid(
                "response exceeds the bounded output buffer",
            ));
        }
        let index = match self.text_index {
            Some(index) => index,
            None => {
                let index = self.allocate_index();
                self.text_index = Some(index);
                self.event(
                    "content_block_start",
                    serde_json::json!({"index":index,
                    "content_block":{"type":"text","text":""}}),
                );
                index
            }
        };
        self.output_bytes = self.output_bytes.saturating_add(text.len());
        self.text.push_str(&text);
        self.event(
            "content_block_delta",
            serde_json::json!({"index":index,
            "delta":{"type":"text_delta","text":text}}),
        );
        Ok(())
    }

    fn tool_delta(
        &mut self,
        engine_index: u32,
        call_id: Option<String>,
        name: Option<String>,
        arguments: String,
    ) -> Result<(), AnthropicError> {
        let tool = self.tools.entry(engine_index).or_default();
        if let Some(call_id) = call_id {
            tool.call_id = call_id;
        }
        if let Some(name) = name {
            tool.name = name;
        }
        if self.output_bytes.saturating_add(arguments.len()) > MAX_COMPLETION_BODY_BYTES {
            return Err(anthropic_invalid(
                "tool input exceeds the bounded output buffer",
            ));
        }
        self.output_bytes = self.output_bytes.saturating_add(arguments.len());
        tool.arguments.push_str(&arguments);
        let announce = tool.block_index.is_none();
        let call_id = tool.call_id.clone();
        let name = tool.name.clone();
        let index = tool.block_index;
        let index = if let Some(index) = index {
            index
        } else {
            let index = self.allocate_index();
            let Some(tool) = self.tools.get_mut(&engine_index) else {
                return Err(anthropic_invalid("upstream tool state is invalid"));
            };
            tool.block_index = Some(index);
            index
        };
        if announce {
            self.event(
                "content_block_start",
                serde_json::json!({"index":index,
                "content_block":{"type":"tool_use","id":call_id,"name":name,"input":{}}}),
            );
        }
        if !arguments.is_empty() {
            self.event(
                "content_block_delta",
                serde_json::json!({"index":index,
                "delta":{"type":"input_json_delta","partial_json":arguments}}),
            );
        }
        Ok(())
    }

    fn allocate_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index = self.next_index.saturating_add(1);
        index
    }

    fn done(&mut self) -> Result<(), AnthropicError> {
        for tool in self.tools.values() {
            if tool.call_id.is_empty()
                || tool.name.is_empty()
                || serde_json::from_str::<Value>(&tool.arguments).is_err()
            {
                return Err(anthropic_invalid("upstream tool input is invalid"));
            }
        }
        let mut indices = self
            .text_index
            .into_iter()
            .chain(self.tools.values().filter_map(|tool| tool.block_index))
            .collect::<Vec<_>>();
        indices.sort_unstable();
        for index in indices {
            self.event("content_block_stop", serde_json::json!({"index":index}));
        }
        self.event("message_delta", serde_json::json!({"delta":{
            "stop_reason":anthropic_stop_reason(self.finish_reason.as_deref()),"stop_sequence":null},
            "usage":{"output_tokens":self.usage.1}}));
        self.event("message_stop", serde_json::json!({}));
        Ok(())
    }
}

fn anthropic_stop_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        Some("stop") | None => "end_turn",
        Some(_) => "stop_sequence",
    }
}

pub struct ResponsesEncoder {
    id: String,
    model: String,
    custom_tools: BTreeSet<String>,
    text: String,
    text_announced: bool,
    tools: BTreeMap<u32, ToolCall>,
    usage: (u64, u64),
    finish_reason: Option<String>,
    sequence: u64,
    pending: VecDeque<OpenAiStreamEvent>,
}

fn invalid_request(message: &'static str) -> OpenAiError {
    OpenAiError {
        code: "invalid_request_error",
        message,
    }
}

#[derive(Default)]
struct ImageBudget {
    count: usize,
    bytes: usize,
}

fn parse_data_uri(value: &str) -> Result<(&str, &str), OpenAiError> {
    let value = value
        .strip_prefix("data:")
        .ok_or_else(|| invalid_request("remote and file image URLs are unsupported"))?;
    let (media_type, data) = value
        .split_once(";base64,")
        .ok_or_else(|| invalid_request("image data URI must contain base64 content"))?;
    if media_type.is_empty() || data.is_empty() {
        return Err(invalid_request("image data URI is empty"));
    }
    Ok((media_type, data))
}

fn decode_image(
    media_type: &str,
    encoded: &str,
    policy: Option<&VisionPolicy>,
    budget: &mut ImageBudget,
) -> Result<ImagePart, OpenAiError> {
    let policy = policy.ok_or_else(|| invalid_request("image input requires a vision recipe"))?;
    if !policy.media_types.contains(media_type)
        || encoded.len() > policy.max_bytes.saturating_add(2) / 3 * 4 + 4
    {
        return Err(invalid_request(
            "image format or encoded size is unsupported",
        ));
    }
    let bytes = BASE64
        .decode(encoded)
        .map_err(|_| invalid_request("image base64 is invalid"))?;
    if bytes.is_empty()
        || bytes.len() > policy.max_bytes
        || budget.count >= policy.max_count
        || budget.bytes.saturating_add(bytes.len()) > policy.max_total_bytes
    {
        return Err(invalid_request("image byte or count limit is exceeded"));
    }
    let format = match media_type {
        "image/png" if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => ImageFormat::Png,
        "image/jpeg" if bytes.starts_with(b"\xff\xd8\xff") => ImageFormat::Jpeg,
        "image/webp"
            if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) =>
        {
            ImageFormat::WebP
        }
        _ => {
            return Err(invalid_request(
                "image media type does not match its magic bytes",
            ))
        }
    };
    let (width, height) = ImageReader::with_format(std::io::Cursor::new(&bytes), format)
        .into_dimensions()
        .map_err(|_| invalid_request("image header is invalid"))?;
    if width == 0 || height == 0 || width > policy.max_width || height > policy.max_height {
        return Err(invalid_request("image dimensions exceed the recipe limit"));
    }
    budget.count += 1;
    budget.bytes += bytes.len();
    Ok(ImagePart {
        media_type: media_type.into(),
        bytes,
        width,
        height,
    })
}

fn canonical_image(image: &ImagePart) -> Value {
    serde_json::json!({
        "type":"image_url",
        "image_url":{"url":format!("data:{};base64,{}", image.media_type, BASE64.encode(&image.bytes))}
    })
}

pub fn vision_health_image(policy: &VisionPolicy) -> Result<ImagePart, OpenAiError> {
    if policy.health_prompt.is_empty()
        || policy.health_prompt.len() > 256
        || policy.health_expected_text.is_empty()
        || policy.health_expected_text.len() > 64
        || !(1..=128).contains(&policy.health_max_tokens)
        || !policy.health_disable_thinking
    {
        return Err(invalid_request("vision health contract is invalid"));
    }
    let mut budget = ImageBudget::default();
    let image = decode_image(
        &policy.health_media_type,
        &policy.health_image_base64,
        Some(policy),
        &mut budget,
    )?;
    if format!("{:x}", Sha256::digest(&image.bytes)) != policy.health_image_sha256 {
        return Err(invalid_request("vision health image identity changed"));
    }
    Ok(image)
}

impl ResponsesEncoder {
    fn custom_input(arguments: &str) -> String {
        serde_json::from_str::<Value>(arguments)
            .ok()
            .and_then(|value| value["input"].as_str().map(str::to_owned))
            .unwrap_or_else(|| arguments.to_owned())
    }

    pub fn new(model: String, custom_tools: BTreeSet<String>) -> Self {
        let id = format!(
            "resp_{}",
            ulid::Ulid::new().to_string().to_ascii_lowercase()
        );
        let mut encoder = Self {
            id,
            model,
            custom_tools,
            text: String::new(),
            text_announced: false,
            tools: BTreeMap::new(),
            usage: (0, 0),
            finish_reason: None,
            sequence: 0,
            pending: VecDeque::new(),
        };
        encoder.push("response.created", "in_progress");
        encoder.push("response.in_progress", "in_progress");
        encoder
    }

    pub fn pop(&mut self) -> Option<OpenAiStreamEvent> {
        self.pending.pop_front()
    }

    pub fn fail(&mut self) {
        let sequence = self.next_sequence();
        self.pending.push_back(OpenAiStreamEvent { name: "response.failed",
            data: serde_json::json!({"type":"response.failed","response":{
                "id":self.id,"object":"response","status":"failed","model":self.model,
                "output":self.output_items(),"error":{"code":"server_error","message":"upstream stream failed"}},
                "sequence_number":sequence}) });
    }

    pub fn final_document(&self) -> Value {
        self.document(if self.finish_reason.as_deref() == Some("length") {
            "incomplete"
        } else {
            "completed"
        })
    }

    pub fn accept(&mut self, event: GenerationEvent) -> Result<(), OpenAiError> {
        match event {
            GenerationEvent::TextDelta { text } => self.text_delta(text),
            GenerationEvent::ToolCallDelta {
                index,
                call_id,
                name,
                arguments,
            } => self.tool_delta(index, call_id, name, arguments),
            GenerationEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.usage = (prompt_tokens, completion_tokens);
                Ok(())
            }
            GenerationEvent::Finished { finish_reason } => {
                self.finish_reason = finish_reason;
                Ok(())
            }
            GenerationEvent::Done => self.done(),
        }
    }

    fn push(&mut self, name: &'static str, status: &str) {
        let sequence = self.next_sequence();
        self.pending.push_back(OpenAiStreamEvent {
            name,
            data: serde_json::json!({"type":name,"sequence_number":sequence,"response":self.document(status)}),
        });
    }

    fn event(&mut self, name: &'static str, fields: Value) {
        let mut data = fields.as_object().cloned().unwrap_or_default();
        data.insert("type".into(), Value::String(name.into()));
        data.insert("sequence_number".into(), Value::from(self.next_sequence()));
        self.pending.push_back(OpenAiStreamEvent {
            name,
            data: Value::Object(data),
        });
    }

    fn next_sequence(&mut self) -> u64 {
        let current = self.sequence;
        self.sequence += 1;
        current
    }

    fn document(&self, status: &str) -> Value {
        serde_json::json!({"id":self.id,"object":"response","created_at":chrono::Utc::now().timestamp(),
            "status":status,"model":self.model,"output":self.output_items(),
            "usage":{"input_tokens":self.usage.0,"output_tokens":self.usage.1,
                "total_tokens":self.usage.0.saturating_add(self.usage.1)},"error":null,
            "incomplete_details":if status == "incomplete" { Some(serde_json::json!({"reason":"max_output_tokens"})) } else { None }})
    }

    fn output_items(&self) -> Vec<Value> {
        let mut output = Vec::new();
        if self.text_announced {
            output.push(serde_json::json!({"id":format!("msg_{}", &self.id[5..]),
            "type":"message","status":"completed","role":"assistant","content":[{
                "type":"output_text","text":self.text,"annotations":[]}]}));
        }
        output.extend(self.tools.values().map(|tool| {
            if self.custom_tools.contains(&tool.name) {
                serde_json::json!({"id":format!("ctc_{}",tool.call_id),"type":"custom_tool_call","status":"completed","call_id":tool.call_id,"name":tool.name,"input":Self::custom_input(&tool.arguments)})
            } else {
                serde_json::json!({"id":format!("fc_{}",tool.call_id),"type":"function_call","status":"completed","call_id":tool.call_id,"name":tool.name,"arguments":tool.arguments})
            }
        }));
        output
    }

    fn text_delta(&mut self, text: String) -> Result<(), OpenAiError> {
        if self.text.len().saturating_add(text.len()) > MAX_COMPLETION_BODY_BYTES {
            return Err(invalid_request(
                "response exceeds the bounded output buffer",
            ));
        }
        if !self.text_announced {
            let item = serde_json::json!({"id":format!("msg_{}", &self.id[5..]),"type":"message",
                "status":"in_progress","role":"assistant","content":[]});
            self.event(
                "response.output_item.added",
                serde_json::json!({"output_index":0,"item":item}),
            );
            self.event("response.content_part.added", serde_json::json!({"item_id":format!("msg_{}", &self.id[5..]),
                "output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}));
            self.text_announced = true;
        }
        self.text.push_str(&text);
        self.event("response.output_text.delta", serde_json::json!({"item_id":format!("msg_{}", &self.id[5..]),"output_index":0,"content_index":0,"delta":text}));
        Ok(())
    }

    fn tool_delta(
        &mut self,
        index: u32,
        call_id: Option<String>,
        name: Option<String>,
        arguments: String,
    ) -> Result<(), OpenAiError> {
        let tool = self.tools.entry(index).or_default();
        if let Some(call_id) = call_id {
            tool.call_id = call_id;
        }
        if let Some(name) = name {
            tool.name = name;
        }
        if tool.arguments.len().saturating_add(arguments.len()) > MAX_COMPLETION_BODY_BYTES {
            return Err(invalid_request(
                "tool arguments exceed the bounded output buffer",
            ));
        }
        tool.arguments.push_str(&arguments);
        let (announce, call_id, name) = (!tool.announced, tool.call_id.clone(), tool.name.clone());
        tool.announced = true;
        if announce {
            self.announce_tool(index, &call_id, &name);
        }
        if !self.custom_tools.contains(&name) && !arguments.is_empty() {
            self.event("response.function_call_arguments.delta", serde_json::json!({"item_id":format!("fc_{call_id}"),"output_index":index,"delta":arguments}));
        }
        Ok(())
    }

    fn announce_tool(&mut self, index: u32, call_id: &str, name: &str) {
        let custom = self.custom_tools.contains(name);
        let item = if custom {
            serde_json::json!({"id":format!("ctc_{call_id}"),"type":"custom_tool_call",
                "status":"in_progress","call_id":call_id,"name":name,"input":""})
        } else {
            serde_json::json!({"id":format!("fc_{call_id}"),"type":"function_call",
                "status":"in_progress","call_id":call_id,"name":name,"arguments":""})
        };
        self.event(
            "response.output_item.added",
            serde_json::json!({"output_index":index,"item":item}),
        );
    }

    fn finish_text(&mut self) {
        if !self.text_announced {
            return;
        }
        let item_id = format!("msg_{}", &self.id[5..]);
        let part = serde_json::json!({"type":"output_text","text":self.text,"annotations":[]});
        self.event("response.output_text.done", serde_json::json!({"item_id":item_id,"output_index":0,"content_index":0,"text":self.text}));
        self.event("response.content_part.done", serde_json::json!({"item_id":item_id,"output_index":0,"content_index":0,"part":part.clone()}));
        let item = serde_json::json!({"id":item_id,"type":"message","status":"completed","role":"assistant","content":[part]});
        self.event(
            "response.output_item.done",
            serde_json::json!({"output_index":0,"item":item}),
        );
    }

    fn finish_tools(&mut self) {
        let tools = self
            .tools
            .iter()
            .map(|(index, tool)| {
                (
                    *index,
                    tool.call_id.clone(),
                    tool.name.clone(),
                    tool.arguments.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (index, call_id, name, arguments) in tools {
            if self.custom_tools.contains(&name) {
                let input = Self::custom_input(&arguments);
                self.event("response.custom_tool_call_input.done", serde_json::json!({"item_id":format!("ctc_{call_id}"),"output_index":index,"input":input}));
                let item = serde_json::json!({"id":format!("ctc_{call_id}"),"type":"custom_tool_call","status":"completed","call_id":call_id,"name":name,"input":input});
                self.event(
                    "response.output_item.done",
                    serde_json::json!({"output_index":index,"item":item}),
                );
            } else {
                self.finish_function(index, &call_id, &name, &arguments);
            }
        }
    }

    fn finish_function(&mut self, index: u32, call_id: &str, name: &str, arguments: &str) {
        self.event(
            "response.function_call_arguments.done",
            serde_json::json!({
            "item_id":format!("fc_{call_id}"),"output_index":index,"arguments":arguments}),
        );
        let item = serde_json::json!({"id":format!("fc_{call_id}"),"type":"function_call",
            "status":"completed","call_id":call_id,"name":name,"arguments":arguments});
        self.event(
            "response.output_item.done",
            serde_json::json!({"output_index":index,"item":item}),
        );
    }

    fn done(&mut self) -> Result<(), OpenAiError> {
        self.finish_text();
        self.finish_tools();
        let incomplete = self.finish_reason.as_deref() == Some("length");
        self.push(
            if incomplete {
                "response.incomplete"
            } else {
                "response.completed"
            },
            if incomplete {
                "incomplete"
            } else {
                "completed"
            },
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicAction {
    Models,
    Completions,
    Chat,
    Responses,
    Embeddings,
}

#[derive(Debug, Clone)]
pub struct HealthyRoute {
    pub generation: u64,
    pub public_model: String,
    pub served_model: String,
    pub upstream: ObservedRoute,
    pub profile: GatewayProfile,
    inference: Arc<tokio::sync::Semaphore>,
}

impl HealthyRoute {
    pub async fn acquire(&self) -> Result<tokio::sync::OwnedSemaphorePermit, OpenAiError> {
        Arc::clone(&self.inference)
            .acquire_owned()
            .await
            .map_err(|_| OpenAiError {
                code: "server_error",
                message: "instance concurrency limiter is unavailable",
            })
    }
}

#[derive(Debug, Clone)]
enum RouteState {
    Warming { generation: u64 },
    Healthy(Arc<HealthyRoute>),
}

#[derive(Debug, Clone)]
pub enum RouteLookup {
    Missing,
    Warming,
    Healthy(Arc<HealthyRoute>),
}

#[derive(Clone)]
pub struct RouteRegistry {
    snapshot: Arc<ArcSwap<BTreeMap<String, RouteState>>>,
}

impl Default for RouteRegistry {
    fn default() -> Self {
        Self {
            snapshot: Arc::new(ArcSwap::from_pointee(BTreeMap::new())),
        }
    }
}

impl RouteRegistry {
    pub fn mark_warming(&self, instance: &str, generation: u64) {
        self.update(instance, RouteState::Warming { generation });
    }

    #[cfg(test)]
    pub fn publish(
        &self,
        instance: &str,
        public_model: String,
        served_model: String,
        upstream: ObservedRoute,
    ) {
        self.publish_with_profile(
            instance,
            public_model,
            served_model,
            GatewayProfile::text(),
            upstream,
        );
    }

    pub fn publish_with_profile(
        &self,
        instance: &str,
        public_model: String,
        served_model: String,
        profile: GatewayProfile,
        upstream: ObservedRoute,
    ) {
        let generation = upstream.identity().1;
        self.update(
            instance,
            RouteState::Healthy(Arc::new(HealthyRoute {
                generation,
                public_model,
                served_model,
                upstream,
                profile,
                inference: Arc::new(tokio::sync::Semaphore::new(MAX_INSTANCE_CONCURRENCY)),
            })),
        );
    }

    pub fn drain(&self, instance: &str, generation: u64) {
        let mut next = (*self.snapshot.load_full()).clone();
        let matches = next.get(instance).is_some_and(|state| match state {
            RouteState::Warming {
                generation: current,
            } => *current == generation,
            RouteState::Healthy(route) => route.generation == generation,
        });
        if matches {
            next.remove(instance);
            self.snapshot.store(Arc::new(next));
        }
    }

    pub fn lookup(&self, instance: &str) -> RouteLookup {
        match self.snapshot.load().get(instance) {
            None => RouteLookup::Missing,
            Some(RouteState::Warming { .. }) => RouteLookup::Warming,
            Some(RouteState::Healthy(route)) => RouteLookup::Healthy(Arc::clone(route)),
        }
    }

    fn update(&self, instance: &str, state: RouteState) {
        let mut next = (*self.snapshot.load_full()).clone();
        next.insert(instance.into(), state);
        self.snapshot.store(Arc::new(next));
    }
}

pub fn public_action(method: &str, suffix: &str) -> Option<PublicAction> {
    match (method, suffix) {
        ("GET", "models") => Some(PublicAction::Models),
        ("POST", "completions") => Some(PublicAction::Completions),
        ("POST", "chat/completions") => Some(PublicAction::Chat),
        ("POST", "responses") => Some(PublicAction::Responses),
        ("POST", "embeddings") => Some(PublicAction::Embeddings),
        _ => None,
    }
}

pub fn models_document(route: &HealthyRoute) -> Value {
    serde_json::json!({
        "object": "list",
        "data": [{
            "id": route.public_model,
            "object": "model",
            "owned_by": "sy-spark"
        }]
    })
}

pub fn rewrite_embeddings_request(
    bytes: &[u8],
    served_model: &str,
    profile: &GatewayProfile,
) -> Result<EmbeddingsRequest, OpenAiError> {
    if bytes.len() > MAX_COMPLETION_BODY_BYTES || !profile.allows(PublicAction::Embeddings) {
        return Err(invalid_request(
            "instance does not provide bounded text embeddings",
        ));
    }
    let policy = profile
        .embeddings
        .as_ref()
        .ok_or_else(|| invalid_request("embedding policy is unavailable"))?;
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| invalid_request("embedding request JSON is invalid"))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_request("embedding request must be an object"))?;
    const KEYS: &[&str] = &["model", "input", "encoding_format", "dimensions", "user"];
    if object.keys().any(|key| !KEYS.contains(&key.as_str()))
        || object
            .get("model")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || object
            .get("encoding_format")
            .is_some_and(|value| value.as_str() != Some("float"))
        || object
            .get("dimensions")
            .is_some_and(|value| value.as_u64() != Some(policy.dimensions as u64))
        || object.get("user").is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|user| user.is_empty() || user.len() > 256)
        })
    {
        return Err(invalid_request("embedding options are unsupported"));
    }
    let inputs = match object.get("input") {
        Some(Value::String(input)) => vec![input.clone()],
        Some(Value::Array(inputs)) => inputs
            .iter()
            .map(|input| {
                input
                    .as_str()
                    .filter(|input| !input.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| invalid_request("embedding inputs must be non-empty strings"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(invalid_request(
                "embedding input must be a string or string array",
            ))
        }
    };
    if inputs.is_empty()
        || inputs.len() > policy.max_batch
        || inputs.iter().map(String::len).sum::<usize>() > policy.max_input_bytes
    {
        return Err(invalid_request(
            "embedding input batch exceeds the recipe limit",
        ));
    }
    let input = if inputs.len() == 1 && object["input"].is_string() {
        Value::String(inputs[0].clone())
    } else {
        serde_json::to_value(&inputs).map_err(|_| invalid_request("embedding input is invalid"))?
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "model":served_model,
        "input":input,
        "encoding_format":"float",
    }))
    .map_err(|_| invalid_request("embedding request cannot be encoded"))?;
    Ok(EmbeddingsRequest {
        body,
        input_count: inputs.len(),
    })
}

pub fn rewrite_embeddings_response(
    bytes: &[u8],
    public_model: &str,
    served_model: &str,
    profile: &GatewayProfile,
    expected_count: usize,
) -> Result<Value, OpenAiError> {
    let policy = profile
        .embeddings
        .as_ref()
        .filter(|_| profile.allows(PublicAction::Embeddings))
        .ok_or_else(|| invalid_request("embedding policy is unavailable"))?;
    let batch = decode_embedding_response(
        bytes,
        served_model,
        expected_count,
        policy.dimensions,
        policy.normalized,
        policy.normalization_tolerance_ppm,
    )
    .map_err(|_| invalid_request("upstream embedding contract is invalid"))?;
    Ok(serde_json::json!({
        "object":"list",
        "model":public_model,
        "data":batch.vectors.into_iter().map(|vector| serde_json::json!({
            "object":"embedding","index":vector.index,"embedding":vector.values
        })).collect::<Vec<_>>(),
        "usage":{"prompt_tokens":batch.prompt_tokens,"total_tokens":batch.total_tokens}
    }))
}

pub fn rewrite_completion_request(bytes: &[u8], served_model: &str) -> Result<(Vec<u8>, bool), ()> {
    if bytes.len() > MAX_COMPLETION_BODY_BYTES {
        return Err(());
    }
    let mut value: Value = serde_json::from_slice(bytes).map_err(|_| ())?;
    let object = value.as_object_mut().ok_or(())?;
    object.insert("model".into(), Value::String(served_model.into()));
    let streaming = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    serde_json::to_vec(&value)
        .map(|bytes| (bytes, streaming))
        .map_err(|_| ())
}

pub fn rewrite_completion_response(bytes: &[u8], public_model: &str) -> Result<Value, ()> {
    let mut value: Value = serde_json::from_slice(bytes).map_err(|_| ())?;
    value
        .as_object_mut()
        .ok_or(())?
        .insert("model".into(), Value::String(public_model.into()));
    Ok(value)
}

pub fn rewrite_chat_request(
    bytes: &[u8],
    served_model: &str,
) -> Result<GenerationRequest, OpenAiError> {
    if bytes.len() > MAX_COMPLETION_BODY_BYTES {
        return Err(invalid_request("request body is too large"));
    }
    let mut value: Value =
        serde_json::from_slice(bytes).map_err(|_| invalid_request("request JSON is invalid"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid_request("request must be an object"))?;
    validate_chat_options(object)?;
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_request("messages must be an array"))?;
    if messages.iter().any(|message| {
        message["content"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                matches!(part["type"].as_str(), Some("image_url" | "input_image"))
                    || part.get("image_url").is_some()
                    || part.get("file_id").is_some()
            })
        })
    }) {
        return Err(invalid_request(
            "chat image inputs are unsupported; use the Responses route",
        ));
    }
    let stream = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    object.insert("model".into(), Value::String(served_model.into()));
    object.insert(
        "stream_options".into(),
        serde_json::json!({"include_usage":true}),
    );
    let body =
        serde_json::to_vec(&value).map_err(|_| invalid_request("request cannot be encoded"))?;
    Ok(GenerationRequest {
        body,
        stream,
        custom_tools: BTreeSet::new(),
    })
}

pub fn rewrite_anthropic_request(
    bytes: &[u8],
    served_model: &str,
) -> Result<GenerationRequest, AnthropicError> {
    rewrite_anthropic_request_with_profile(bytes, served_model, &GatewayProfile::text())
}

pub fn rewrite_anthropic_request_with_profile(
    bytes: &[u8],
    served_model: &str,
    profile: &GatewayProfile,
) -> Result<GenerationRequest, AnthropicError> {
    if bytes.len() > MAX_COMPLETION_BODY_BYTES {
        return Err(anthropic_invalid("request body is too large"));
    }
    let request: AnthropicMessageRequest =
        serde_json::from_slice(bytes).map_err(|_| anthropic_invalid("request JSON is invalid"))?;
    if request.model.is_empty()
        || request.messages.is_empty()
        || request.max_tokens == 0
        || request.max_tokens > MAX_OUTPUT_TOKENS
    {
        return Err(anthropic_invalid("model or output token limit is invalid"));
    }
    validate_anthropic_options(&request)?;
    let mut messages = Vec::new();
    if let Some(system) = request.system {
        messages
            .push(serde_json::json!({"role":"system","content":anthropic_system_text(system)?}));
    }
    messages.extend(anthropic_messages(
        request.messages,
        profile.vision.as_ref(),
    )?);
    let parallel_tool_calls = request
        .tool_choice
        .as_ref()
        .is_none_or(|choice| !choice.disable_parallel_tool_use);
    let tools = anthropic_tools(request.tools)?;
    let tool_choice = anthropic_tool_choice(request.tool_choice)?;
    let body = serde_json::to_vec(&serde_json::json!({
        "model":served_model,"messages":messages,"max_tokens":request.max_tokens,
        "stream":true,"stream_options":{"include_usage":true},"tools":tools,
        "tool_choice":tool_choice,"parallel_tool_calls":parallel_tool_calls,
        "stop":request.stop_sequences,"temperature":request.temperature,
        "top_p":request.top_p,"top_k":request.top_k
    }))
    .map_err(|_| anthropic_invalid("request cannot be encoded"))?;
    Ok(GenerationRequest {
        body,
        stream: request.stream,
        custom_tools: BTreeSet::new(),
    })
}

pub fn rewrite_anthropic_count_request(
    bytes: &[u8],
    served_model: &str,
) -> Result<Vec<u8>, AnthropicError> {
    if bytes.len() > MAX_COMPLETION_BODY_BYTES {
        return Err(anthropic_invalid("request body is too large"));
    }
    let mut value: Value =
        serde_json::from_slice(bytes).map_err(|_| anthropic_invalid("request JSON is invalid"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anthropic_invalid("request must be an object"))?;
    const KEYS: &[&str] = &[
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "thinking",
        "context_management",
        "output_config",
        "metadata",
        "service_tier",
    ];
    if object.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(anthropic_invalid("count request contains an unknown field"));
    }
    object.insert("max_tokens".into(), Value::from(1));
    object.insert("stream".into(), Value::Bool(false));
    let synthetic = serde_json::to_vec(&value)
        .map_err(|_| anthropic_invalid("count request cannot be encoded"))?;
    let generation = rewrite_anthropic_request(&synthetic, served_model)?;
    let generation: Value = serde_json::from_slice(&generation.body)
        .map_err(|_| anthropic_invalid("count request cannot be encoded"))?;
    serde_json::to_vec(&serde_json::json!({
        "model":served_model,"messages":generation["messages"],
        "tools":generation["tools"],"add_generation_prompt":true
    }))
    .map_err(|_| anthropic_invalid("count request cannot be encoded"))
}

pub fn rewrite_anthropic_count_response(bytes: &[u8]) -> Result<u64, AnthropicError> {
    if bytes.len() > MAX_COMPLETION_BODY_BYTES {
        return Err(anthropic_invalid("tokenizer response is too large"));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| anthropic_invalid("tokenizer response is invalid"))?;
    let count = value["count"]
        .as_u64()
        .ok_or_else(|| anthropic_invalid("tokenizer response is invalid"))?;
    Ok(count)
}

fn anthropic_invalid(message: &'static str) -> AnthropicError {
    AnthropicError {
        error_type: "invalid_request_error",
        message,
    }
}

fn anthropic_system_text(content: AnthropicContent) -> Result<String, AnthropicError> {
    match content {
        AnthropicContent::Text(text) => Ok(text),
        AnthropicContent::Blocks(blocks) => {
            blocks
                .into_iter()
                .try_fold(String::new(), |mut text, block| {
                    let AnthropicContentBlock::Text {
                        text: part,
                        cache_control,
                    } = block
                    else {
                        return Err(anthropic_invalid("system content must contain only text"));
                    };
                    validate_cache_control(cache_control.as_ref())?;
                    text.push_str(&part);
                    Ok(text)
                })
        }
    }
}

fn validate_anthropic_options(request: &AnthropicMessageRequest) -> Result<(), AnthropicError> {
    if request.top_k.is_some_and(|value| value == 0)
        || request
            .temperature
            .is_some_and(|value| !(0.0..=1.0).contains(&value))
        || request
            .top_p
            .is_some_and(|value| !(0.0..=1.0).contains(&value))
        || request
            .metadata
            .as_ref()
            .and_then(|value| value.user_id.as_ref())
            .is_some_and(|value| value.len() > 256)
        || request
            .service_tier
            .as_deref()
            .is_some_and(|value| value != "auto")
    {
        return Err(anthropic_invalid("request options are invalid"));
    }
    if let Some(thinking) = &request.thinking {
        if !matches!(thinking.kind.as_str(), "adaptive" | "disabled")
            || thinking.budget_tokens.is_some()
            || thinking
                .display
                .as_deref()
                .is_some_and(|value| value != "omitted")
        {
            return Err(anthropic_invalid("thinking mode is unsupported"));
        }
    }
    if request.context_management.as_ref().is_some_and(|context| {
        context
            .edits
            .iter()
            .any(|edit| edit.kind != "clear_thinking_20251015" || edit.keep != "all")
    }) || request
        .output_config
        .as_ref()
        .is_some_and(|output| !matches!(output.effort.as_str(), "low" | "medium" | "high" | "max"))
    {
        return Err(anthropic_invalid(
            "Claude Code compatibility option is invalid",
        ));
    }
    Ok(())
}

fn validate_cache_control(value: Option<&AnthropicCacheControl>) -> Result<(), AnthropicError> {
    if value.is_some_and(|value| {
        value.kind != "ephemeral"
            || value
                .ttl
                .as_deref()
                .is_some_and(|ttl| !matches!(ttl, "5m" | "1h"))
    }) {
        Err(anthropic_invalid("cache control is invalid"))
    } else {
        Ok(())
    }
}

fn anthropic_tools(tools: Vec<AnthropicTool>) -> Result<Vec<Value>, AnthropicError> {
    tools
        .into_iter()
        .map(|tool| {
            validate_cache_control(tool.cache_control.as_ref())?;
            if tool.kind.is_some() || tool.name.is_empty() || tool.name.len() > 128 {
                return Err(AnthropicError {
                    error_type: "invalid_request_error",
                    message: "provider-hosted tools are unsupported",
                });
            }
            if !tool.input_schema.is_object() {
                return Err(anthropic_invalid("tool input_schema must be an object"));
            }
            Ok(serde_json::json!({"type":"function","function":{
                "name":tool.name,"description":tool.description,"parameters":tool.input_schema}}))
        })
        .collect()
}

fn anthropic_tool_choice(choice: Option<AnthropicToolChoice>) -> Result<Value, AnthropicError> {
    let Some(choice) = choice else {
        return Ok(Value::String("auto".into()));
    };
    match choice.kind.as_str() {
        "auto" => Ok(Value::String("auto".into())),
        "any" => Ok(Value::String("required".into())),
        "none" => Ok(Value::String("none".into())),
        "tool" => choice
            .name
            .filter(|name| !name.is_empty())
            .map(|name| serde_json::json!({"type":"function","function":{"name":name}}))
            .ok_or_else(|| anthropic_invalid("tool choice name is required")),
        _ => Err(anthropic_invalid("tool choice is unsupported")),
    }
}

fn anthropic_messages(
    messages: Vec<AnthropicMessage>,
    vision: Option<&VisionPolicy>,
) -> Result<Vec<Value>, AnthropicError> {
    let mut output = Vec::new();
    let mut pending = BTreeSet::new();
    let mut images = ImageBudget::default();
    for message in messages {
        if !matches!(message.role.as_str(), "user" | "assistant") {
            return Err(anthropic_invalid("message role is invalid"));
        }
        match message.content {
            AnthropicContent::Text(text) => {
                output.push(serde_json::json!({"role":message.role,"content":text}))
            }
            AnthropicContent::Blocks(blocks) => anthropic_blocks(
                &message.role,
                blocks,
                &mut pending,
                &mut output,
                vision,
                &mut images,
            )?,
        }
    }
    if pending.is_empty() {
        Ok(output)
    } else {
        Err(anthropic_invalid("tool_use is missing its tool_result"))
    }
}

fn anthropic_blocks(
    role: &str,
    blocks: Vec<AnthropicContentBlock>,
    pending: &mut BTreeSet<String>,
    output: &mut Vec<Value>,
    vision: Option<&VisionPolicy>,
    images: &mut ImageBudget,
) -> Result<(), AnthropicError> {
    let mut content = Vec::new();
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for block in blocks {
        match block {
            AnthropicContentBlock::Text {
                text: part,
                cache_control,
            } => {
                validate_cache_control(cache_control.as_ref())?;
                content.push(serde_json::json!({"type":"text","text":part}));
            }
            AnthropicContentBlock::ToolUse {
                id,
                name,
                input,
                cache_control,
            } => {
                validate_cache_control(cache_control.as_ref())?;
                if role != "assistant"
                    || id.is_empty()
                    || name.is_empty()
                    || !pending.insert(id.clone())
                {
                    return Err(anthropic_invalid("tool_use block is invalid"));
                }
                calls.push(serde_json::json!({"id":id,"type":"function","function":{
                    "name":name,"arguments":input.to_string()}}));
            }
            AnthropicContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                cache_control,
            } => {
                validate_cache_control(cache_control.as_ref())?;
                if role != "user" || !pending.remove(&tool_use_id) {
                    return Err(anthropic_invalid(
                        "tool_result does not match a prior tool_use",
                    ));
                }
                let content = anthropic_tool_result_text(content)?;
                let content = if is_error {
                    format!("Tool error: {content}")
                } else {
                    content
                };
                results.push(serde_json::json!({"role":"tool","tool_call_id":tool_use_id,
                    "content":content}));
            }
            AnthropicContentBlock::Image { source } => {
                if role != "user" || source.kind != "base64" {
                    return Err(anthropic_invalid(
                        "image source must be inline base64 user content",
                    ));
                }
                let image = decode_image(&source.media_type, &source.data, vision, images)
                    .map_err(|error| anthropic_invalid(error.message))?;
                content.push(canonical_image(&image));
            }
        }
    }
    if !content.is_empty() || !calls.is_empty() {
        let content = if content.len() == 1 && content[0]["type"] == "text" {
            content[0]["text"].clone()
        } else if content.is_empty() {
            Value::Null
        } else {
            Value::Array(content)
        };
        let mut message = serde_json::json!({"role":role,"content":content});
        if !calls.is_empty() {
            message["tool_calls"] = Value::Array(calls);
        }
        output.push(message);
    }
    output.extend(results);
    Ok(())
}

fn anthropic_tool_result_text(
    content: AnthropicToolResultContent,
) -> Result<String, AnthropicError> {
    match content {
        AnthropicToolResultContent::Text(text) => Ok(text),
        AnthropicToolResultContent::Blocks(blocks) => {
            blocks
                .into_iter()
                .try_fold(String::new(), |mut text, block| {
                    validate_cache_control(block.cache_control.as_ref())?;
                    if block.kind != "text" {
                        return Err(anthropic_invalid("tool_result content type is unsupported"));
                    }
                    text.push_str(&block.text);
                    Ok(text)
                })
        }
    }
}

fn validate_chat_options(object: &serde_json::Map<String, Value>) -> Result<(), OpenAiError> {
    const ALLOWED: &[&str] = &[
        "model",
        "messages",
        "frequency_penalty",
        "logit_bias",
        "logprobs",
        "top_logprobs",
        "max_completion_tokens",
        "max_tokens",
        "n",
        "presence_penalty",
        "response_format",
        "seed",
        "service_tier",
        "stop",
        "stream",
        "stream_options",
        "temperature",
        "tool_choice",
        "tools",
        "parallel_tool_calls",
        "top_p",
        "user",
        "metadata",
        "reasoning_effort",
        "verbosity",
        "prompt_cache_key",
        "safety_identifier",
    ];
    if object.keys().any(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(invalid_request("request contains an unknown field"));
    }
    if object.get("tools").is_some_and(|tools| {
        tools.as_array().is_none_or(|tools| {
            tools
                .iter()
                .any(|tool| tool["type"].as_str() != Some("function"))
        })
    }) {
        return Err(OpenAiError {
            code: "unsupported_tool",
            message: "hosted tools are not available on this gateway",
        });
    }
    if ["max_tokens", "max_completion_tokens"].iter().any(|key| {
        object
            .get(*key)
            .is_some_and(|value| value.as_u64().is_none_or(|value| value > MAX_OUTPUT_TOKENS))
    }) {
        return Err(invalid_request("output token limit is invalid"));
    }
    Ok(())
}

pub fn rewrite_chat_response(bytes: &[u8], public_model: &str) -> Result<Value, OpenAiError> {
    let mut value: Value = serde_json::from_slice(bytes)
        .map_err(|_| invalid_request("upstream response is invalid"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid_request("upstream response is invalid"))?;
    if !object.get("choices").is_some_and(Value::is_array) {
        return Err(invalid_request("upstream chat choices are invalid"));
    }
    object.insert("model".into(), Value::String(public_model.into()));
    Ok(value)
}

pub fn chat_stream_document(event: GenerationEvent, id: &str, model: &str) -> Option<Value> {
    let (choices, usage) = match event {
        GenerationEvent::TextDelta { text } => (
            serde_json::json!([{"index":0,"delta":{"content":text},"finish_reason":null}]),
            Value::Null,
        ),
        GenerationEvent::ToolCallDelta {
            index,
            call_id,
            name,
            arguments,
        } => (
            serde_json::json!([{"index":0,"delta":{"tool_calls":[{
            "index":index,"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}}]},"finish_reason":null}]),
            Value::Null,
        ),
        GenerationEvent::Finished { finish_reason } => (
            serde_json::json!([{"index":0,"delta":{},"finish_reason":finish_reason}]),
            Value::Null,
        ),
        GenerationEvent::Usage {
            prompt_tokens,
            completion_tokens,
        } => (
            serde_json::json!([]),
            serde_json::json!({
            "prompt_tokens":prompt_tokens,"completion_tokens":completion_tokens,"total_tokens":prompt_tokens.saturating_add(completion_tokens)}),
        ),
        GenerationEvent::Done => return None,
    };
    Some(
        serde_json::json!({"id":id,"object":"chat.completion.chunk","model":model,"choices":choices,"usage":usage}),
    )
}

#[cfg(test)]
pub fn rewrite_responses_request(
    bytes: &[u8],
    served_model: &str,
) -> Result<GenerationRequest, OpenAiError> {
    rewrite_responses_request_with_profile(bytes, served_model, &GatewayProfile::text())
}

pub fn rewrite_responses_request_with_profile(
    bytes: &[u8],
    served_model: &str,
    profile: &GatewayProfile,
) -> Result<GenerationRequest, OpenAiError> {
    if bytes.len() > MAX_COMPLETION_BODY_BYTES {
        return Err(invalid_request("request body is too large"));
    }
    let request: Value =
        serde_json::from_slice(bytes).map_err(|_| invalid_request("request JSON is invalid"))?;
    let object = request
        .as_object()
        .ok_or_else(|| invalid_request("request must be an object"))?;
    validate_response_options(object)?;
    let (tools, custom_tools) = response_tools(object.get("tools"))?;
    let mut messages = response_messages(object.get("input"), profile.vision.as_ref())?;
    if let Some(instructions) = object.get("instructions").and_then(Value::as_str) {
        messages.insert(
            0,
            serde_json::json!({"role": "system", "content": instructions}),
        );
    }
    let messages = merge_system_messages(messages);
    if !messages.iter().any(|message| message["role"] == "user") {
        return Err(invalid_request("stateless input must contain a user query"));
    }
    let max_tokens = response_max_tokens(object.get("max_output_tokens"))?;
    let tool_choice = response_tool_choice(object.get("tool_choice"))?;
    let stream = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let body = serde_json::to_vec(&serde_json::json!({
        "model": served_model, "messages": messages,
        "stream": true, "stream_options": {"include_usage": true}, "tools": tools,
        "max_tokens": max_tokens, "parallel_tool_calls": object.get("parallel_tool_calls").cloned().unwrap_or(Value::Bool(true))
        , "tool_choice": tool_choice
    })).map_err(|_| invalid_request("request cannot be encoded"))?;
    Ok(GenerationRequest {
        body,
        stream,
        custom_tools,
    })
}

/// The pinned Ornith template accepts `system` only at index zero and has no
/// `developer` branch. Public instruction roles therefore become one native
/// system message before user/assistant/tool history is sent to vLLM.
fn merge_system_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut system = Vec::new();
    let mut native = Vec::new();
    for message in messages {
        if message["role"] == "system" {
            if let Some(content) = message["content"].as_str() {
                system.push(content.to_owned());
            }
        } else {
            native.push(message);
        }
    }
    if !system.is_empty() {
        native.insert(
            0,
            serde_json::json!({"role":"system","content":system.join("\n\n")}),
        );
    }
    native
}

fn response_tools(value: Option<&Value>) -> Result<(Vec<Value>, BTreeSet<String>), OpenAiError> {
    let Some(value) = value else {
        return Ok((Vec::new(), BTreeSet::new()));
    };
    let tools = value
        .as_array()
        .ok_or_else(|| invalid_request("tools must be an array"))?;
    if tools.iter().any(|tool| {
        !matches!(
            tool["type"].as_str(),
            Some("function" | "custom" | "namespace")
        )
    }) {
        return Err(OpenAiError {
            code: "unsupported_tool",
            message: "only client-side function and custom tools are supported",
        });
    }
    let mut custom = BTreeSet::new();
    let mut converted = Vec::new();
    for tool in tools {
        if tool["type"] == "namespace" {
            append_namespace_tools(tool, &mut custom, &mut converted)?;
        } else {
            converted.push(response_tool(tool, &mut custom)?);
        }
    }
    Ok((converted, custom))
}

fn response_tool(tool: &Value, custom: &mut BTreeSet<String>) -> Result<Value, OpenAiError> {
    let name = tool["name"]
        .as_str()
        .ok_or_else(|| invalid_request("tool name is required"))?;
    if name.is_empty() || name.len() > 128 {
        return Err(invalid_request("tool name is invalid"));
    }
    let description = tool["description"].as_str().unwrap_or_default();
    let parameters = if tool["type"] == "custom" {
        custom.insert(name.into());
        serde_json::json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"]})
    } else {
        tool.get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type":"object"}))
    };
    Ok(
        serde_json::json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}}),
    )
}

fn append_namespace_tools(
    tool: &Value,
    custom: &mut BTreeSet<String>,
    output: &mut Vec<Value>,
) -> Result<(), OpenAiError> {
    let namespace = tool["name"]
        .as_str()
        .ok_or_else(|| invalid_request("namespace name is required"))?;
    let tools = tool["tools"]
        .as_array()
        .ok_or_else(|| invalid_request("namespace tools are invalid"))?;
    for child in tools {
        if child["type"] != "function" {
            return Err(invalid_request("namespace child is unsupported"));
        }
        let mut child = child.clone();
        let name = child["name"]
            .as_str()
            .ok_or_else(|| invalid_request("namespace tool name is required"))?;
        child["name"] = Value::String(format!("{namespace}.{name}"));
        output.push(response_tool(&child, custom)?);
    }
    Ok(())
}

fn validate_response_options(object: &serde_json::Map<String, Value>) -> Result<(), OpenAiError> {
    const KEYS: &[&str] = &[
        "model",
        "input",
        "instructions",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "max_output_tokens",
        "stream",
        "store",
        "include",
        "reasoning",
        "text",
        "temperature",
        "top_p",
        "truncation",
        "metadata",
        "prompt_cache_key",
        "client_metadata",
        "stream_options",
        "service_tier",
        "safety_identifier",
        "previous_response_id",
        "conversation",
        "background",
    ];
    if object.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(invalid_request("request contains an unknown field"));
    }
    if object.get("store").and_then(Value::as_bool) == Some(true) {
        return Err(invalid_request(
            "server-side response storage is unsupported",
        ));
    }
    if object.get("background").and_then(Value::as_bool) == Some(true)
        || object
            .get("previous_response_id")
            .is_some_and(|value| !value.is_null())
        || object
            .get("conversation")
            .is_some_and(|value| !value.is_null())
    {
        return Err(invalid_request(
            "only stateless synchronous responses are supported",
        ));
    }
    Ok(())
}

fn response_messages(
    value: Option<&Value>,
    vision: Option<&VisionPolicy>,
) -> Result<Vec<Value>, OpenAiError> {
    let value = value.ok_or_else(|| invalid_request("input is required"))?;
    if let Some(text) = value.as_str() {
        return Ok(vec![serde_json::json!({"role": "user", "content": text})]);
    }
    let items = value
        .as_array()
        .ok_or_else(|| invalid_request("input must be a string or item array"))?;
    let mut images = ImageBudget::default();
    items
        .iter()
        .map(|item| response_input_item(item, vision, &mut images))
        .collect()
}

fn response_input_item(
    item: &Value,
    vision: Option<&VisionPolicy>,
    images: &mut ImageBudget,
) -> Result<Value, OpenAiError> {
    match item["type"].as_str() {
        Some("message") => response_message(item, vision, images),
        Some("function_call") => response_tool_call(item, false),
        Some("custom_tool_call") => response_tool_call(item, true),
        Some("function_call_output" | "custom_tool_call_output") => response_tool_output(item),
        Some("reasoning") => Ok(serde_json::json!({"role": "assistant", "content": ""})),
        Some("computer_call" | "computer_call_output") => Err(OpenAiError {
            code: "unsupported_tool",
            message: "hosted computer tools are unsupported",
        }),
        _ => Err(invalid_request("input item type is unsupported")),
    }
}

fn response_message(
    item: &Value,
    vision: Option<&VisionPolicy>,
    images: &mut ImageBudget,
) -> Result<Value, OpenAiError> {
    let role = item["role"]
        .as_str()
        .ok_or_else(|| invalid_request("message role is required"))?;
    if !matches!(role, "system" | "developer" | "user" | "assistant") {
        return Err(invalid_request("message role is invalid"));
    }
    let content = response_content(&item["content"], role, vision, images)?;
    let role = if role == "developer" { "system" } else { role };
    Ok(serde_json::json!({"role": role, "content": content}))
}

fn response_content(
    value: &Value,
    role: &str,
    vision: Option<&VisionPolicy>,
    images: &mut ImageBudget,
) -> Result<Value, OpenAiError> {
    if let Some(text) = value.as_str() {
        return Ok(Value::String(text.into()));
    }
    let parts = value
        .as_array()
        .ok_or_else(|| invalid_request("message content is invalid"))?;
    let mut content = Vec::new();
    for part in parts {
        match part["type"].as_str() {
            Some("input_text" | "output_text") => content.push(serde_json::json!({
                "type":"text",
                "text":part["text"].as_str().ok_or_else(|| invalid_request("text content is invalid"))?
            })),
            Some("input_image") => {
                if role != "user" || part.as_object().is_none_or(|object| {
                    object.keys().any(|key| !matches!(key.as_str(), "type" | "image_url" | "detail"))
                }) || part.get("file_id").is_some() {
                    return Err(invalid_request("image input must be bounded inline user content"));
                }
                let url = part["image_url"]
                    .as_str()
                    .ok_or_else(|| invalid_request("image_url must be an inline data URI"))?;
                let (media_type, data) = parse_data_uri(url)?;
                let image = decode_image(media_type, data, vision, images)?;
                content.push(canonical_image(&image));
            }
            _ => return Err(invalid_request("message content type is unsupported")),
        }
    }
    if content.len() == 1 && content[0]["type"] == "text" {
        Ok(content.remove(0)["text"].clone())
    } else {
        Ok(Value::Array(content))
    }
}

fn response_tool_call(item: &Value, custom: bool) -> Result<Value, OpenAiError> {
    let call_id = item["call_id"]
        .as_str()
        .ok_or_else(|| invalid_request("tool call_id is required"))?;
    let name = item["name"]
        .as_str()
        .ok_or_else(|| invalid_request("tool name is required"))?;
    let arguments = if custom {
        serde_json::json!({"input": item["input"].as_str().ok_or_else(|| invalid_request("custom tool input is required"))?}).to_string()
    } else {
        item["arguments"]
            .as_str()
            .ok_or_else(|| invalid_request("function arguments are required"))?
            .into()
    };
    Ok(
        serde_json::json!({"role":"assistant","content":null,"tool_calls":[{
        "id":call_id,"type":"function","function":{"name":name,"arguments":arguments}}]}),
    )
}

fn response_tool_output(item: &Value) -> Result<Value, OpenAiError> {
    let call_id = item["call_id"]
        .as_str()
        .ok_or_else(|| invalid_request("tool output call_id is required"))?;
    let output = item
        .get("output")
        .ok_or_else(|| invalid_request("tool output is required"))?;
    let content = output
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| output.to_string());
    Ok(serde_json::json!({"role": "tool", "tool_call_id": call_id, "content": content}))
}

fn response_max_tokens(value: Option<&Value>) -> Result<u64, OpenAiError> {
    let tokens = value.and_then(Value::as_u64).unwrap_or(4_096);
    if tokens == 0 || tokens > MAX_OUTPUT_TOKENS {
        Err(invalid_request(
            "max_output_tokens exceeds the recipe ceiling",
        ))
    } else {
        Ok(tokens)
    }
}

fn response_tool_choice(value: Option<&Value>) -> Result<Value, OpenAiError> {
    let Some(value) = value else {
        return Ok(Value::String("auto".into()));
    };
    if value
        .as_str()
        .is_some_and(|choice| matches!(choice, "auto" | "none" | "required"))
    {
        return Ok(value.clone());
    }
    let name = value["name"]
        .as_str()
        .ok_or_else(|| invalid_request("tool_choice is invalid"))?;
    if !matches!(value["type"].as_str(), Some("function" | "custom")) {
        return Err(OpenAiError {
            code: "unsupported_tool",
            message: "hosted tool choice is unsupported",
        });
    }
    Ok(serde_json::json!({"type":"function","function":{"name":name}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(generation: u64) -> ObservedRoute {
        ObservedRoute::new(
            "i_11111111111111111111111111111111",
            generation,
            "172.30.0.2".parse().unwrap(),
            8000,
            [("GET", "/v1/models"), ("POST", "/v1/completions")],
        )
        .unwrap()
    }

    #[test]
    fn route_is_absent_until_semantic_identity_passes() {
        let routes = RouteRegistry::default();
        routes.mark_warming("ornith", 1);
        assert!(matches!(routes.lookup("ornith"), RouteLookup::Warming));
        routes.publish(
            "ornith",
            "ornith-1.5:9b".into(),
            "Ornith-1.5-9B".into(),
            route(1),
        );
        assert!(matches!(routes.lookup("ornith"), RouteLookup::Healthy(_)));
    }

    #[test]
    fn openai_route_allowlist_exposes_only_implemented_generation_protocols() {
        for denied in [
            ("GET", "health"),
            ("GET", "metrics"),
            ("POST", "tokenize"),
            ("GET", "http://169.254.169.254/latest/meta-data"),
            ("DELETE", "models"),
        ] {
            assert_eq!(public_action(denied.0, denied.1), None);
        }
        assert_eq!(public_action("GET", "models"), Some(PublicAction::Models));
        assert_eq!(
            public_action("POST", "completions"),
            Some(PublicAction::Completions)
        );
        assert_eq!(
            public_action("POST", "chat/completions"),
            Some(PublicAction::Chat)
        );
        assert_eq!(
            public_action("POST", "responses"),
            Some(PublicAction::Responses)
        );
    }

    #[test]
    fn draining_old_generation_cannot_remove_new_route() {
        let routes = RouteRegistry::default();
        routes.publish("ornith", "public".into(), "internal".into(), route(2));
        routes.drain("ornith", 1);
        assert!(matches!(routes.lookup("ornith"), RouteLookup::Healthy(_)));
    }

    #[test]
    fn responses_string_input_translates_to_native_vllm_chat() {
        let request = rewrite_responses_request(
            br#"{"model":"ornith","input":"hello","stream":true}"#,
            "Ornith-1.5-9B",
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(value["model"], "Ornith-1.5-9B");
        assert_eq!(value["messages"][0]["content"], "hello");
        assert!(request.stream);
    }

    #[test]
    fn responses_rejects_hosted_tools_instead_of_forwarding_them() {
        let error = rewrite_responses_request(
            br#"{"model":"ornith","input":"hello","tools":[{"type":"web_search_preview"}]}"#,
            "Ornith-1.5-9B",
        )
        .unwrap_err();
        assert_eq!(error.code, "unsupported_tool");
    }

    #[test]
    fn responses_text_sse_lifecycle_is_exact() {
        let mut encoder = ResponsesEncoder::new("ornith".into(), BTreeSet::new());
        encoder
            .accept(GenerationEvent::TextDelta { text: "hi".into() })
            .unwrap();
        encoder
            .accept(GenerationEvent::Finished {
                finish_reason: Some("stop".into()),
            })
            .unwrap();
        encoder.accept(GenerationEvent::Done).unwrap();
        let names = std::iter::from_fn(|| encoder.pop())
            .map(|event| event.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed"
            ]
        );
    }

    #[test]
    fn chat_completions_rewrites_only_identity_over_the_shared_model() {
        let request = rewrite_chat_request(
            br#"{"model":"public","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
            "Ornith-1.5-9B",
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(value["messages"][0]["content"], "hi");
        assert_eq!(value["model"], "Ornith-1.5-9B");
        assert!(
            rewrite_chat_request(br#"{"messages":[],"url":"http://private"}"#, "model").is_err()
        );
        assert!(rewrite_chat_request(
            br#"{"messages":[],"tools":[{"type":"web_search"}]}"#,
            "model"
        )
        .is_err());
    }

    #[test]
    fn codex_0_149_payload_metadata_and_namespaces_stay_client_side() {
        let request = rewrite_responses_request(br#"{"model":"ornith","instructions":"root rules","input":[{"type":"message","role":"developer","content":[{"type":"input_text","text":"developer rules"}]},{"type":"message","role":"system","content":"system rules"},{"type":"message","role":"user","content":"run it"},{"type":"function_call","call_id":"call_1","name":"run","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"done"}],"tools":[{"type":"namespace","name":"multi","tools":[{"type":"function","name":"run","parameters":{"type":"object"}}]}],"store":false,"stream":true,"client_metadata":{"thread_id":"fixture"}}"#,
            "Ornith-1.5-9B").unwrap();
        let value: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(value["tools"][0]["function"]["name"], "multi.run");
        assert!(value.get("client_metadata").is_none());
        let roles = value["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(roles, ["system", "user", "assistant", "tool"]);
        let system = value["messages"][0]["content"].as_str().unwrap();
        assert!(
            system.contains("root rules")
                && system.contains("developer rules")
                && system.contains("system rules")
        );
    }

    #[test]
    fn anthropic_and_openai_adapters_share_events_not_json() {
        let events = [
            GenerationEvent::TextDelta { text: "hi".into() },
            GenerationEvent::Usage {
                prompt_tokens: 2,
                completion_tokens: 1,
            },
            GenerationEvent::Finished {
                finish_reason: Some("stop".into()),
            },
            GenerationEvent::Done,
        ];
        let mut openai = ResponsesEncoder::new("ornith".into(), BTreeSet::new());
        let mut anthropic = AnthropicEncoder::new("ornith".into());
        for event in events {
            openai.accept(event.clone()).unwrap();
            anthropic.accept(event).unwrap();
        }
        assert_eq!(openai.final_document()["object"], "response");
        assert_eq!(anthropic.final_document()["type"], "message");
    }

    #[test]
    fn anthropic_hosted_tools_beta_and_unknown_routes_reject() {
        for body in [
            br#"{"model":"ornith","messages":[{"role":"user","content":"x"}],"max_tokens":8,"tools":[{"type":"web_search_20250305","name":"web","input_schema":{}}]}"#.as_slice(),
            br#"{"model":"ornith","messages":[{"role":"user","content":"x"}],"max_tokens":8,"container":{"id":"hosted"}}"#.as_slice(),
            br#"{"model":"ornith","messages":[{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://example"}}]}],"max_tokens":8}"#.as_slice(),
        ] {
            assert!(rewrite_anthropic_request(body, "Ornith-1.5-9B").is_err());
        }
    }
}
