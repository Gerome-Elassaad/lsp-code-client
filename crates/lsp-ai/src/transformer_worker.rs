use anyhow::Context;
use lsp_server::{Connection, Message, RequestId, Response};
use lsp_types::{
    CodeAction, CodeActionParams, CompletionItem, CompletionItemKind, CompletionList,
    CompletionParams, CompletionResponse, Position, Range, TextDocumentIdentifier,
    TextDocumentPositionParams, TextEdit, WorkspaceEdit,
};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{mpsc::RecvTimeoutError, Arc},
    time::{Duration, SystemTime},
};
use tokio::sync::oneshot;
use tracing::{error, info, instrument};

use crate::config::{self, Config};
use crate::custom_requests::generation::{GenerateResult, GenerationParams};
use crate::custom_requests::generation_stream::GenerationStreamParams;
use crate::memory_backends::Prompt;
use crate::memory_worker::{self, FileRequest, FilterRequest, PromptRequest};
use crate::transformer_backends::TransformerBackend;
use crate::utils::{ToResponseError, TOKIO_RUNTIME};

static RE: Lazy<Mutex<HashMap<String, Regex>>> = Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug)]
pub(crate) struct CompletionRequest {
    id: RequestId,
    params: CompletionParams,
}

impl CompletionRequest {
    pub(crate) fn new(id: RequestId, params: CompletionParams) -> Self {
        Self { id, params }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GenerationRequest {
    id: RequestId,
    params: GenerationParams,
}

impl GenerationRequest {
    pub(crate) fn new(id: RequestId, params: GenerationParams) -> Self {
        Self { id, params }
    }
}

// The generate stream is not yet ready but we don't want to remove it
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct GenerationStreamRequest {
    id: RequestId,
    params: GenerationStreamParams,
}

impl GenerationStreamRequest {
    pub(crate) fn new(id: RequestId, params: GenerationStreamParams) -> Self {
        Self { id, params }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CodeActionRequest {
    id: RequestId,
    params: CodeActionParams,
}

impl CodeActionRequest {
    pub(crate) fn new(id: RequestId, params: CodeActionParams) -> Self {
        Self { id, params }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CodeActionResolveRequest {
    id: RequestId,
    params: CodeAction,
}

impl CodeActionResolveRequest {
    pub(crate) fn new(id: RequestId, params: CodeAction) -> Self {
        Self { id, params }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum WorkerRequest {
    Shutdown,
    Completion(CompletionRequest),
    Generation(GenerationRequest),
    GenerationStream(GenerationStreamRequest),
    CodeActionRequest(CodeActionRequest),
    CodeActionResolveRequest(CodeActionResolveRequest),
}

impl WorkerRequest {
    fn get_id(&self) -> RequestId {
        match self {
            WorkerRequest::Shutdown => unreachable!(),
            WorkerRequest::Completion(r) => r.id.clone(),
            WorkerRequest::Generation(r) => r.id.clone(),
            WorkerRequest::GenerationStream(r) => r.id.clone(),
            WorkerRequest::CodeActionRequest(r) => r.id.clone(),
            WorkerRequest::CodeActionResolveRequest(r) => r.id.clone(),
        }
    }
}

pub(crate) struct DoCompletionResponse {
    pub(crate) insert_text: String,
}

pub(crate) struct DoGenerationResponse {
    pub(crate) generated_text: String,
}

#[allow(dead_code)]
pub(crate) struct DoGenerationStreamResponse {
    pub(crate) generated_text: String,
}

fn post_process_start(response: String, front: &str) -> String {
    let response_chars: Vec<char> = response.chars().collect();
    let front_chars: Vec<char> = front.chars().collect();

    let mut front_match = response_chars.len();
    loop {
        if response_chars.is_empty() || front_chars.ends_with(&response_chars[..front_match]) {
            break;
        } else {
            front_match = front_match.saturating_sub(1);
        }
    }

    if front_match > 0 {
        response_chars[front_match..].iter().collect()
    } else {
        response
    }
}

fn post_process_end(response: String, back: &str) -> String {
    let response_chars: Vec<char> = response.chars().collect();
    let back_chars: Vec<char> = back.chars().collect();

    let mut back_match = 0;
    loop {
        if back_match == response_chars.len()
            || back_chars.starts_with(&response_chars[back_match..])
        {
            break;
        } else {
            back_match += 1;
        }
    }

    if back_match > 0 {
        response_chars[..back_match].iter().collect()
    } else {
        response
    }
}

// Some basic post processing that will clean up duplicate characters at the front and back
fn post_process_response(
    response: String,
    prompt: &Prompt,
    config: &config::PostProcess,
) -> String {
    match prompt {
        Prompt::ContextAndCode(context_and_code) => {
            // First we need to extract
            let response = if let Some(extractor) = &config.extractor {
                let mut re_map = RE.lock();
                let re = match re_map.get(extractor) {
                    Some(re) => re,
                    None => {
                        let re = Regex::new(extractor).unwrap();
                        re_map.insert(extractor.to_owned(), re);
                        re_map.get(extractor).unwrap()
                    }
                };
                let response = re
                    .captures(&response)
                    .and_then(|cap| cap.get(1))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                info!("response text after extracting:\n{}", response);
                response
            } else {
                response
            };
            if context_and_code.code.contains("<CURSOR>") {
                let mut split = context_and_code.code.split("<CURSOR>");
                let response = if config.remove_duplicate_start {
                    post_process_start(response, split.next().unwrap())
                } else {
                    response
                };
                if config.remove_duplicate_end {
                    post_process_end(response, split.next().unwrap())
                } else {
                    response
                }
            } else if config.remove_duplicate_start {
                post_process_start(response, &context_and_code.code)
            } else {
                response
            }
        }
        Prompt::FIM(fim) => {
            let response = if config.remove_duplicate_start {
                post_process_start(response, &fim.prompt)
            } else {
                response
            };
            if config.remove_duplicate_end {
                post_process_end(response, &fim.suffix)
            } else {
                response
            }
        }
    }
}

pub(crate) fn run(
    transformer_backends: HashMap<String, Box<dyn TransformerBackend + Send + Sync>>,
    memory_tx: std::sync::mpsc::Sender<memory_worker::WorkerRequest>,
    transformer_rx: std::sync::mpsc::Receiver<WorkerRequest>,
    connection: Arc<Connection>,
    config: Config,
) {
    if let Err(e) = do_run(
        transformer_backends,
        memory_tx,
        transformer_rx,
        connection,
        config,
    ) {
        error!("error in transformer worker: {e:?}")
    }
}

fn do_run(
    transformer_backends: HashMap<String, Box<dyn TransformerBackend + Send + Sync>>,
    memory_backend_tx: std::sync::mpsc::Sender<memory_worker::WorkerRequest>,
    transformer_rx: std::sync::mpsc::Receiver<WorkerRequest>,
    connection: Arc<Connection>,
    config: Config,
) -> anyhow::Result<()> {
    let transformer_backends = Arc::new(transformer_backends);

    // If this errors completion is disabled
    let max_requests_per_second = config.get_completion_transformer_max_requests_per_second();
    let mut last_completion_request_time = SystemTime::now();
    let mut last_completion_request = None;

    let run_dispatch_request = |request| {
        let task_connection = connection.clone();
        let task_transformer_backends = transformer_backends.clone();
        let task_memory_backend_tx = memory_backend_tx.clone();
        let task_config = config.clone();
        TOKIO_RUNTIME.spawn(async move {
            dispatch_request(
                request,
                task_connection,
                task_transformer_backends,
                task_memory_backend_tx,
                task_config,
            )
            .await;
        });
    };

    loop {
        // We want to rate limit completions without dropping the last rate limited request
        let request = transformer_rx.recv_timeout(Duration::from_millis(5));

        match request {
            Ok(request) => match &request {
                WorkerRequest::Shutdown => {
                    return Ok(());
                }
                WorkerRequest::Completion(completion_request) => {
                    if max_requests_per_second.is_ok() {
                        last_completion_request = Some(request);
                    } else {
                        // If completion is disabled return an empty response
                        let completion_list = CompletionList {
                            is_incomplete: false,
                            items: vec![],
                        };
                        let result = Some(CompletionResponse::List(completion_list));
                        let result = serde_json::to_value(result).unwrap();
                        if let Err(e) = connection.sender.send(Message::Response(Response {
                            id: completion_request.id.clone(),
                            result: Some(result),
                            error: None,
                        })) {
                            error!("sending empty response for completion request: {e:?}");
                        }
                    }
                }
                _ => run_dispatch_request(request),
            },
            Err(RecvTimeoutError::Disconnected) => anyhow::bail!("channel disconnected"),
            _ => {}
        }

        if let Ok(max_requests_per_second) = max_requests_per_second {
            if SystemTime::now()
                .duration_since(last_completion_request_time)?
                .as_secs_f32()
                < 1. / max_requests_per_second
            {
                continue;
            }

            if let Some(request) = last_completion_request.take() {
                last_completion_request_time = SystemTime::now();
                run_dispatch_request(request);
            }
        }
    }
}

#[instrument(skip(connection, transformer_backends, memory_backend_tx, config))]
async fn dispatch_request(
    request: WorkerRequest,
    connection: Arc<Connection>,
    transformer_backends: Arc<HashMap<String, Box<dyn TransformerBackend + Send + Sync>>>,
    memory_backend_tx: std::sync::mpsc::Sender<memory_worker::WorkerRequest>,
    config: Config,
) {
    let response = match generate_response(
        request.clone(),
        transformer_backends,
        memory_backend_tx,
        config,
    )
    .await
    {
        Ok(response) => response,
        Err(e) => {
            error!("generating response: {e:?}");
            Response {
                id: request.get_id(),
                result: None,
                error: Some(e.to_response_error(-32603)),
            }
        }
    };

    if let Err(e) = connection.sender.send(Message::Response(response)) {
        error!("sending response: {e:?}");
    }
}

async fn generate_response(
    request: WorkerRequest,
    transformer_backends: Arc<HashMap<String, Box<dyn TransformerBackend + Send + Sync>>>,
    memory_backend_tx: std::sync::mpsc::Sender<memory_worker::WorkerRequest>,
    config: Config,
) -> anyhow::Result<Response> {
    let model_name = match &request {
        WorkerRequest::Completion(request) => {
            let completion_config = config
                .config
                .completion
                .as_ref()
                .context("Completions is none")?;
            config.config.model_selection.request_type_model.get("completion").unwrap_or(&completion_config.model).clone()
        }
        WorkerRequest::Generation(request) => {
            config.config.model_selection.request_type_model.get("generation").unwrap_or(&request.params.model).clone()
        }
        WorkerRequest::GenerationStream(_) => {
            anyhow::bail!("Streaming is not yet supported")
        }
        WorkerRequest::CodeActionRequest(_) => {
            config.config.model_selection.request_type_model.get("code_action").unwrap_or(&config.config.completion.as_ref().context("Completions is none")?.model).clone()
        }
        WorkerRequest::CodeActionResolveRequest(_) => {
            config.config.model_selection.request_type_model.get("code_action_resolve").unwrap_or(&config.config.completion.as_ref().context("Completions is none")?.model).clone()
        }
        WorkerRequest::Shutdown => unreachable!(),
    };

    let transformer_backend = transformer_backends
        .get(&model_name)
        .with_context(|| format!("can't find model: {}", &model_name))?;

    match request {
        WorkerRequest::Completion(request) => {
            do_completion(transformer_backend, memory_backend_tx, &request, &config).await
        }
        WorkerRequest::Generation(request) => {
            do_generate(transformer_backend, memory_backend_tx, &request).await
        }
        WorkerRequest::GenerationStream(_) => {
            anyhow::bail!("Streaming is not yet supported")
        }
        WorkerRequest::CodeActionRequest(request) => {
            do_code_action_request(memory_backend_tx, &request, &config).await
        }
        WorkerRequest::CodeActionResolveRequest(request) => {
            do_code_action_resolve(transformer_backends, memory_backend_tx, &request, &config).await
        }
        WorkerRequest::Shutdown => unreachable!(),
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct CodeActionResolveData {
    text_document: TextDocumentIdentifier,
    range: Range,
}

async fn do_chat_code_action_resolve(
    action: &config::Chat,
    transformer_backends: Arc<HashMap<String, Box<dyn TransformerBackend + Send + Sync>>>,
    memory_backend_tx: std::sync::mpsc::Sender<memory_worker::WorkerRequest>,
    request: &CodeActionResolveRequest,
) -> anyhow::Result<CodeAction> {
    let transformer_backend = transformer_backends.get(&action.model).with_context(|| {
        format!(
            "model: {} not found when resolving code action",
            action.model
        )
    })?;

    let data: CodeActionResolveData = serde_json::from_value(
        request
            .params
            .data
            .clone()
            .context("the `data` field is required to resolve a code action")?,
    )
    .context("the `data` field could not be deserialized when resolving the code action")?;

    // Get the file
    let (tx, rx) = oneshot::channel();
    memory_backend_tx.send(memory_worker::WorkerRequest::File(FileRequest::new(
        TextDocumentIdentifier {
            uri: data.text_document.uri.clone(),
        },
        tx,
    )))?;
    let file_text = rx.await?;

    let (messages_text, text_edit_line, text_edit_char) = if action.trigger == "" {
        (
            file_text.as_str(),
            file_text.lines().count(),
            file_text.lines().last().unwrap_or("").chars().count(),
        )
    } else {
        let mut split = file_text.splitn(2, &action.trigger);
        let text_edit_line = split
            .next()
            .context("trigger not found when resolving chat code action")?
            .lines()
            .count();
        let messages_text = split
            .next()
            .context("trigger not found when resolving chat code action")?;
        (
            messages_text,
            text_edit_line + messages_text.lines().count(),
            messages_text.lines().last().unwrap_or("").chars().count(),
        )
    };

    // Parse into messages
    // NOTE: We are making some asumptions about the parameters the endpoint takes
    // Some APIs like Gemini do not take the messages in this format. We should add
    // some kind of configuration option for this
    let mut new_messages = vec![];
    let mut current_message = String::new();
    let mut is_user = true;
    for line in messages_text.lines() {
        if is_user && line.contains("<|assistant|>") {
            new_messages.push(serde_json::json!({
                "role": "user",
                "content": current_message
            }));
            current_message = String::new();
            is_user = false;
        } else if !is_user && line.contains("<|user|>") {
            new_messages.push(serde_json::json!({
                "role": "assistant",
                "content": current_message
            }));
            current_message = String::new();
            is_user = true;
        } else {
            current_message += line;
        }
    }
    if current_message.len() > 0 {
        if is_user {
            new_messages.push(serde_json::json!({
                "role": "user",
                "content": current_message
            }));
        } else {
            new_messages.push(serde_json::json!({
                "role": "assistant",
                "content": current_message
            }));
        }
    }

    // Add the messages to the params messages
    // NOTE: Once again we are making some assumptions that the messages key is even the right key to use here
    let mut params = action.parameters.clone();
    if let Some(messages) = params.get_mut("messages") {
        messages
            .as_array_mut()
            .context("`messages` key must be an array")?
            .append(&mut new_messages);
    } else {
        params.insert(
            "messages".to_string(),
            serde_json::to_value(&new_messages).unwrap(),
        );
    }

    let params = serde_json::to_value(&params).unwrap();

    // Build the prompt
    let (tx, rx) = oneshot::channel();
    memory_backend_tx.send(memory_worker::WorkerRequest::Prompt(PromptRequest::new(
        TextDocumentPositionParams {
            text_document: data.text_document.clone(),
            position: data.range.start,
        },
        transformer_backend.get_prompt_type(&params)?,
        params.clone(),
        tx,
    )))?;
    let prompt = rx.await?;

    // Get the response
    let mut response = transformer_backend.do_completion(&prompt, params).await?;
    response.insert_text = format!("\n\n<|assistant|>\n{}\n\n<|user|>\n", response.insert_text);

    let edit = TextEdit::new(
        Range::new(
            Position::new(text_edit_line as u32, text_edit_char as u32),
            Position::new(text_edit_line as u32, text_edit_char as u32),
        ),
        response.insert_text.clone(),
    );
    let changes = HashMap::from([(data.text_document.uri, vec![edit])]);

    Ok(CodeAction {
        title: action.action_display_name.clone(),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

async fn do_code_action_action_resolve(
    action: &config::Action,
    transformer_backends: Arc<HashMap<String, Box<dyn TransformerBackend + Send + Sync>>>,
    memory_backend_tx: std::sync::mpsc::Sender<memory_worker::WorkerRequest>,
    request: &CodeActionResolveRequest,
) -> anyhow::Result<CodeAction> {
    let transformer_backend = transformer_backends.get(&action.model).with_context(|| {
        format!(
            "model: {} not found when resolving code action",
            action.model
        )
    })?;

    let data: CodeActionResolveData = serde_json::from_value(
        request
            .params
            .data
            .clone()
            .context("the `data` field is required to resolve a code action")?,
    )
    .context("the `data` field could not be deserialized when resolving the code action")?;

    let params = serde_json::to_value(action.parameters.clone()).unwrap();

    // Get the prompt
    let text_document_position = TextDocumentPositionParams {
        text_document: data.text_document.clone(),
        position: data.range.start,
    };
    let (tx, rx) = oneshot::channel();
    memory_backend_tx.send(memory_worker::WorkerRequest::Prompt(PromptRequest::new(
        text_document_position,
        transformer_backend.get_prompt_type(&params)?,
        params.clone(),
        tx,
    )))?;
    let mut prompt = rx.await?;

    // If they have some text highlighted and we aren't doing FIM  let's get it
    if matches!(prompt, Prompt::ContextAndCode(_)) && data.range.start != data.range.end {
        // Get the file
        let (tx, rx) = oneshot::channel();
        memory_backend_tx.send(memory_worker::WorkerRequest::File(FileRequest::new(
            TextDocumentIdentifier {
                uri: data.text_document.uri.clone(),
            },
            tx,
        )))?;
        let file_text = rx.await?;

        // Get the text
        let lines: Vec<&str> = file_text.lines().collect();
        let mut result = String::new();
        for (i, line) in lines
            .iter()
            .enumerate()
            .skip(data.range.start.line as usize)
            .take((data.range.end.line - data.range.start.line + 1) as usize)
        {
            let start_char = if i == data.range.start.line as usize {
                data.range.start.character as usize
            } else {
                0
            };
            let end_char = if i == data.range.end.line as usize {
                data.range.end.character as usize + 1
            } else {
                line.len()
            };

            if start_char < line.len() {
                result.push_str(&line[start_char..end_char.min(line.len())]);
            }

            if i != data.range.end.line as usize {
                result.push('\n');
            }
        }

        // Update our prompt to include the selected text
        if let Prompt::ContextAndCode(prompt) = &mut prompt {
            prompt.selected_text = Some(result)
        }
    }

    // Get the response
    let mut response = transformer_backend.do_completion(&prompt, params).await?;
    response.insert_text =
        post_process_response(response.insert_text, &prompt, &action.post_process);

    let edit = TextEdit::new(
        Range::new(
            Position::new(data.range.start.line, data.range.start.character),
            Position::new(data.range.end.line, data.range.end.character),
        ),
        response.insert_text.clone(),
    );
    let changes = HashMap::from([(data.text_document.uri, vec![edit])]);

    Ok(CodeAction {
        title: action.action_display_name.clone(),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

// TODO: @silas we need to make this compatible with any llm backend
async fn do_code_action_resolve(
    transformer_backends: Arc<HashMap<String, Box<dyn TransformerBackend + Send + Sync>>>,
    memory_backend_tx: std::sync::mpsc::Sender<memory_worker::WorkerRequest>,
    request: &CodeActionResolveRequest,
    config: &Config,
) -> anyhow::Result<Response> {
    let model_name = config.config.model_selection.request_type_model.get("code_action_resolve").unwrap_or(&config.config.completion.as_ref().context("Completions is none")?.model).clone();

    let transformer_backend = transformer_backends
        .get(&model_name)
        .with_context(|| format!("can't find model: {}", &model_name))?;

    let action = if let Some(chat_action) = config
        .get_chats()
        .iter()
        .find(|chat_action| chat_action.action_display_name == request.params.title)
    {
        do_chat_code_action_resolve(
            chat_action,
            transformer_backends,
            memory_backend_tx,
            request,
        )
        .await?
    } else {
        let action = config
            .get_actions()
            .iter()
            .find(|action| action.action_display_name == request.params.title)
            .with_context(|| {
                format!(
                    "action: {} does not exist in `chats` or `actions`",
                    request.params.title
                )
            })?;
        do_code_action_action_resolve(transformer_backend, memory_backend_tx, request, config)
            .await?
    };
    Ok(Response {
        id: request.id.clone(),
        result: Some(serde_json::to_value(action).unwrap()),
        error: None,
    })
}

async fn do_code_action_request(
    memory_backend_tx: std::sync::mpsc::Sender<memory_worker::WorkerRequest>,
    request: &CodeActionRequest,
    config: &Config,
) -> anyhow::Result<Response> {
    let actions = config.get_actions();
    let chats = config.get_chats();

    let enabled_chats = futures::future::join_all(chats.iter().map(|chat| async {
        let (tx, rx) = oneshot::channel();
        memory_backend_tx
            .clone()
            .send(memory_worker::WorkerRequest::CodeActionRequest(
                memory_worker::CodeActionRequest::new(
                    request.params.text_document.clone(),
                    request.params.range,
                    chat.trigger.clone(),
                    tx,
                ),
            ))?;
        anyhow::Ok(rx.await?)
    }))
    .await
    .into_iter()
    .collect::<anyhow::Result<Vec<bool>>>()?;

    let mut code_actions: Vec<CodeAction> = chats
        .into_iter()
        .zip(enabled_chats)
        .filter(|(_, is_enabled)| *is_enabled)
        .map(|(chat, _)| CodeAction {
            title: chat.action_display_name.to_owned(),
            data: Some(
                serde_json::to_value(CodeActionResolveData {
                    text_document: request.params.text_document.clone(),
                    range: request.params.range,
                })
                .unwrap(),
            ),
            ..Default::default()
        })
        .collect();

    code_actions.extend(actions.into_iter().map(|action| {
        CodeAction {
            title: action.action_display_name.to_owned(),
            data: Some(
                serde_json::to_value(CodeActionResolveData {
                    text_document: request.params.text_document.clone(),
                    range: request.params.range,
                })
                .unwrap(),
            ),
            ..Default::default()
        }
    }));

    Ok(Response {
        id: request.id.clone(),
        result: Some(serde_json::to_value(&code_actions).unwrap()),
        error: None,
    })
}

async fn do_completion(
    transformer_backend: &Box<dyn TransformerBackend + Send + Sync>,
    memory_backend_tx: std::sync::mpsc::Sender<memory_worker::WorkerRequest>,
    request: &CompletionRequest,
    config: &Config,
) -> anyhow::Result<Response> {
    let params = serde_json::to_value(
        config
            .config
            .completion
            .as_ref()
            .context("Completions is None")?
            .parameters
            .clone(),
    )
    .unwrap();

    // Build the prompt
    let (tx, rx) = oneshot::channel();
    memory_backend_tx.send(memory_worker::WorkerRequest::Prompt(PromptRequest::new(
        request.params.text_document_position.clone(),
        transformer_backend.get_prompt_type(&params)?,
        params.clone(),
        tx,
    )))?;
    let prompt = rx.await?;

    // Get the filter text
    let (tx, rx) = oneshot::channel();
    memory_backend_tx.send(memory_worker::WorkerRequest::FilterText(
        FilterRequest::new(request.params.text_document_position.clone(), tx),
    ))?;
    let filter_text = rx.await?;

    // Get the response
    let mut response = transformer_backend.do_completion(&prompt, params).await?;

    if let Some(post_process) = config.get_completions_post_process() {
        response.insert_text = post_process_response(response.insert_text, &prompt, post_process);
    }

    // Build and send the response
    let completion_text_edit = TextEdit::new(
        Range::new(
            Position::new(
                request.params.text_document_position.position.line,
                request.params.text_document_position.position.character,
            ),
            Position::new(
                request.params.text_document_position.position.line,
                request.params.text_document_position.position.character,
            ),
        ),
        response.insert_text.clone(),
    );
    let item = CompletionItem {
        label: format!("ai - {}", response.insert_text),
        filter_text: Some(filter_text),
        text_edit: Some(lsp_types::CompletionTextEdit::Edit(completion_text_edit)),
        kind: Some(CompletionItemKind::TEXT),
        ..Default::default()
    };
    let completion_list = CompletionList {
        is_incomplete: false,
        items: vec![item],
    };
    let result = Some(CompletionResponse::List(completion_list));
    let result = serde_json::to_value(result).unwrap();
    Ok(Response {
        id: request.id.clone(),
        result: Some(result),
        error: None,
    })
}

async fn do_generate(
    transformer_backend: &Box<dyn TransformerBackend + Send + Sync>,
    memory_backend_tx: std::sync::mpsc::Sender<memory_worker::WorkerRequest>,
    request: &GenerationRequest,
) -> anyhow::Result<Response> {
    let params = serde_json::to_value(request.params.parameters.clone()).unwrap();

    let (tx, rx) = oneshot::channel();
    memory_backend_tx.send(memory_worker::WorkerRequest::Prompt(PromptRequest::new(
        request.params.text_document_position.clone(),
        transformer_backend.get_prompt_type(&params)?,
        params.clone(),
        tx,
    )))?;
    let prompt = rx.await?;

    let mut response = transformer_backend.do_generate(&prompt, params).await?;
    response.generated_text = post_process_response(
        response.generated_text,
        &prompt,
        &request.params.post_process,
    );

    let result = GenerateResult {
        generated_text: response.generated_text,
    };
    let result = serde_json::to_value(result).unwrap();
    Ok(Response {
        id: request.id.clone(),
        result: Some(result),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_backends::{
        file_store::FileStore, ContextAndCodePrompt, FIMPrompt, MemoryBackend,
    };
    use serde_json::json;
    use std::{sync::mpsc, thread};

    #[tokio::test]
    async fn test_do_completion() -> anyhow::Result<()> {
        let (memory_tx, memory_rx) = mpsc::channel();
        let memory_backend: Box<dyn MemoryBackend + Send + Sync> =
            Box::new(FileStore::default_with_filler_file()?);
        thread::spawn(move || memory_worker::run(memory_backend, memory_rx));

        let transformer_backend: Box<dyn TransformerBackend + Send + Sync> =
            config::ValidModel::Ollama(serde_json::from_value(
                json!({"model": "deepseek-coder:1.3b-base"}),
            )?)
            .try_into()?;
        let completion_request = CompletionRequest::new(
            serde_json::from_value(json!(0))?,
            serde_json::from_value(json!({
                "position": {"character":10, "line":2},
                "textDocument": {
                    "uri": "file:///filler.py"
                }
            }))?,
        );
        let mut config = config::Config::default_with_file_store_without_models();
        config.config.completion = Some(serde_json::from_value(json!({
            "model": "model1",
            "parameters": {
                "options": {
                    "temperature": 0
                }
            }
        }))?);

        let result = do_completion(
            &transformer_backend,
            memory_tx,
            &completion_request,
            &config,
        )
        .await?;

        assert_eq!(
            " x * y",
            result.result.clone().unwrap()["items"][0]["textEdit"]["newText"]
                .as_str()
                .unwrap()
        );
        assert_eq!(
            "    return",
            result.result.unwrap()["items"][0]["filterText"]
                .as_str()
                .unwrap()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_do_generate() -> anyhow::Result<()> {
        let (memory_tx, memory_rx) = mpsc::channel();
        let memory_backend: Box<dyn MemoryBackend + Send + Sync> =
            Box::new(FileStore::default_with_filler_file()?);
        thread::spawn(move || memory_worker::run(memory_backend, memory_rx));

        let transformer_backend: Box<dyn TransformerBackend + Send + Sync> =
            config::ValidModel::Ollama(serde_json::from_value(
                json!({"model": "deepseek-coder:1.3b-base"}),
            )?)
            .try_into()?;
        let generation_request = GenerationRequest::new(
            serde_json::from_value(json!(0))?,
            serde_json::from_value(json!({
                "position": {"character":10, "line":2},
                "textDocument": {
                    "uri": "file:///filler.py"
                },
                "model": "model1",
                "parameters": {
                    "options": {
                        "temperature": 0
                    }
                }
            }))?,
        );
        let result = do_generate(&transformer_backend, memory_tx, &generation_request).await?;

        assert_eq!(
            " x * y",
            result.result.unwrap()["generatedText"].as_str().unwrap()
        );

        Ok(())
    }

    #[test]
    fn test_post_process_fim() {
        let config = config::PostProcess::default();

        let prompt = Prompt::FIM(FIMPrompt {
            prompt: "test 1234 ".to_string(),
            suffix: "ttabc".to_string(),
        });
        let response = "4 zz tta".to_string();
        let new_response = post_process_response(response.clone(), &prompt, &config);
        assert_eq!(new_response, "zz ");

        let prompt = Prompt::FIM(FIMPrompt {
            prompt: "test".to_string(),
            suffix: "test".to_string(),
        });
        let response = "zzzz".to_string();
        let new_response = post_process_response(response.clone(), &prompt, &config);
        assert_eq!(new_response, "zzzz");
    }

    #[test]
    fn test_post_process_context_and_code() {
        let config = config::PostProcess::default();

        let prompt = Prompt::ContextAndCode(ContextAndCodePrompt {
            context: "".to_string(),
            code: "tt ".to_string(),
            selected_text: None,
        });
        let response = "tt abc".to_string();
        let new_response = post_process_response(response.clone(), &prompt, &config);
        assert_eq!(new_response, "abc");

        let prompt = Prompt::ContextAndCode(ContextAndCodePrompt {
            context: "".to_string(),
            code: "ff".to_string(),
            selected_text: None,
        });
        let response = "zz".to_string();
        let new_response = post_process_response(response.clone(), &prompt, &config);
        assert_eq!(new_response, "zz");

        let prompt = Prompt::ContextAndCode(ContextAndCodePrompt {
            context: "".to_string(),
            code: "tt <CURSOR> tt".to_string(),
            selected_text: None,
        });
        let response = "tt abc tt".to_string();
        let new_response = post_process_response(response.clone(), &prompt, &config);
        assert_eq!(new_response, "abc");

        let prompt = Prompt::ContextAndCode(ContextAndCodePrompt {
            context: "".to_string(),
            code: "d<CURSOR>d".to_string(),
            selected_text: None,
        });
        let response = "zz".to_string();
        let new_response = post_process_response(response.clone(), &prompt, &config);
        assert_eq!(new_response, "zz");
    }
}
