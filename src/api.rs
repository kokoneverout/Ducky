use crate::config::{Config, Provider, RecognitionMode};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

const MIMO: &str = "https://api.xiaomimimo.com/v1/chat/completions";
const OPENROUTER: &str = "https://openrouter.ai/api/v1/responses";
const MAX_RESPONSE: usize = 2 * 1024 * 1024;

/// Worker-owned runtime. Cancellation drops the HTTP future and its response body.
pub struct Client {
    http: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}
pub fn client() -> Result<Client> {
    Ok(Client {
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()?,
        runtime: tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
    })
}
impl Client {
    fn run<T>(
        &self,
        future: impl Future<Output = Result<T>>,
        cancel: Option<&async_channel::Receiver<()>>,
    ) -> Result<T> {
        self.runtime.block_on(async {
            if let Some(cancel) = cancel {
                futures_lite::future::or(future, async {
                    let _ = cancel.recv().await;
                    bail!("已取消测试")
                })
                .await
            } else {
                future.await
            }
        })
    }
}
pub fn completion_url(base: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(base.trim()).context("接口地址格式错误")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "接口地址必须使用 http 或 https"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "接口地址不能包含凭据、查询参数或片段"
    );
    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with("/chat/completions") {
        path.to_string()
    } else if path.ends_with("/chat") {
        format!("{path}/completions")
    } else {
        format!("{path}/chat/completions")
    };
    url.set_path(&path);
    Ok(url.to_string())
}
pub fn transcription_body(config: &Config, wav: &[u8]) -> Value {
    let encoded = STANDARD.encode(wav);
    if config.recognition_mode == RecognitionMode::Asr {
        return json!({"model":"mimo-v2.5-asr", "stream":config.asr_stream,
        "asr_options":{"language":config.asr_language.code()},
        "messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{
            "data":format!("data:audio/wav;base64,{encoded}")
        }}]}]});
    }
    if config.provider == Provider::Openrouter {
        // Current OpenRouter audio documentation uses Responses with raw base64.
        return json!({"model":config.openrouter_model.trim(),"stream":config.openrouter_stream,
        "input":[{"role":"user","content":[
            {"type":"input_text","text":config.openrouter_prompt},
            {"type":"input_audio","input_audio":{"data":encoded,"format":"wav"}}
        ]}]});
    }
    json!({"model":config.mimo_model.trim(),"stream":config.mimo_stream,
    "thinking":{"type":"disabled"},"messages":[{"role":"user","content":[
        {"type":"input_audio","input_audio":{"data":format!("data:audio/wav;base64,{encoded}"),"format":"wav"}},
        {"type":"text","text":config.transcription_prompt}
    ]}]})
}
pub fn transcribe(client: &Client, config: &Config, samples: &[i16]) -> Result<String> {
    transcribe_wav(
        client,
        config,
        &crate::audio::wav(samples),
        None,
        &mut |_| {},
    )
}
pub fn transcribe_wav(
    client: &Client,
    config: &Config,
    wav: &[u8],
    cancel: Option<&async_channel::Receiver<()>>,
    progress: &mut dyn FnMut(String),
) -> Result<String> {
    config.validate_recognition()?;
    ensure!(
        wav.len().div_ceil(3) * 4 <= 10_000_000,
        "编码后的音频超过 10 MB，请缩短录音"
    );
    let router = config.recognition_mode == RecognitionMode::Multimodal
        && config.provider == Provider::Openrouter;
    let body = transcription_body(config, wav);
    client.run(
        complete(
            &client.http,
            if router { OPENROUTER } else { MIMO },
            if router {
                &config.openrouter_key
            } else {
                &config.mimo_key
            },
            !router,
            &body,
            router,
            progress,
        ),
        cancel,
    )
}
pub fn polish(client: &Client, config: &Config, text: &str) -> Result<String> {
    polish_with_cancel(client, config, text, None)
}
pub fn polish_with_cancel(
    client: &Client,
    config: &Config,
    text: &str,
    cancel: Option<&async_channel::Receiver<()>>,
) -> Result<String> {
    config.validate_polish()?;
    let url = completion_url(&config.polish_url)?;
    let body = json!({"model":config.polish_model.trim(),"stream":false,"messages":[
        {"role":"system","content":config.polish_prompt},{"role":"user","content":text}
    ]});
    client.run(
        complete(
            &client.http,
            &url,
            &config.polish_key,
            false,
            &body,
            false,
            &mut |_| {},
        ),
        cancel,
    )
}
fn network_error(e: reqwest::Error) -> anyhow::Error {
    if e.is_timeout() {
        anyhow::anyhow!("模型请求超时，请检查网络或模型服务")
    } else {
        anyhow::anyhow!("网络请求失败，请检查网络和接口地址")
    }
}
fn check_status(response: &reqwest::Response) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let reason = match status.as_u16() {
        401 | 403 => "认证失败，请检查 API Key 和账户权限",
        402 => "额度不足，请检查账户余额",
        404 => "模型或接口不存在，请检查模型 ID",
        429 => "请求受限，请检查额度或稍后重试",
        400 | 422 => "请求不受支持，请检查模型是否支持音频及所选参数",
        _ => "服务暂时不可用，请稍后重试",
    };
    // A provider's raw error may echo credentials/audio; never display it.
    bail!("HTTP {}：{reason}", status.as_u16())
}
async fn read_json(mut response: reqwest::Response, limit: usize) -> Result<Value> {
    check_status(&response)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        ensure!(bytes.len() + chunk.len() <= limit, "接口响应过大");
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).context("接口未返回有效 JSON")?;
    ensure!(
        value.get("error").is_none(),
        "服务返回错误，请检查模型、权限及参数"
    );
    Ok(value)
}
#[allow(clippy::too_many_arguments)]
async fn complete(
    http: &reqwest::Client,
    url: &str,
    key: &str,
    mimo: bool,
    body: &Value,
    responses: bool,
    progress: &mut dyn FnMut(String),
) -> Result<String> {
    let mut request = http.post(url).json(body);
    if !key.trim().is_empty() {
        request = if mimo {
            request.header("api-key", key.trim())
        } else {
            request.bearer_auth(key.trim())
        };
    }
    let mut response = request.send().await.map_err(network_error)?;
    check_status(&response)?;
    if body["stream"] != true {
        let value = read_json(response, MAX_RESPONSE).await?;
        return if responses {
            extract_response(&value)
        } else {
            extract_chat(&value)
        };
    }
    ensure!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream")),
        "流式请求未返回 SSE，请检查服务或关闭流式"
    );
    let mut decoder = Sse::default();
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        decoder.feed(&chunk, responses, progress)?;
        if decoder.done {
            break;
        }
    }
    ensure!(decoder.done, "流式响应中途断开，未使用不完整文本");
    ensure!(
        !decoder.text.trim().is_empty(),
        "模型返回空正文，这不代表录音没有人声"
    );
    Ok(decoder.text.trim().into())
}
fn valid_finish(value: &Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    ensure!(
        value == "stop",
        "模型输出未正常完成（截断、过滤或非文本响应），请检查模型配置"
    );
    Ok(())
}
fn extract_chat(value: &Value) -> Result<String> {
    let choice = value["choices"]
        .as_array()
        .and_then(|v| v.first())
        .context("接口未返回 choices")?;
    valid_finish(&choice["finish_reason"])?;
    let text = if let Some(s) = choice["message"]["content"].as_str() {
        s.to_owned()
    } else {
        choice["message"]["content"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<String>()
            })
            .unwrap_or_default()
    };
    ensure!(
        !text.trim().is_empty(),
        "模型返回空正文，这不代表录音没有人声"
    );
    Ok(text.trim().into())
}
fn extract_response(value: &Value) -> Result<String> {
    ensure!(
        value["status"] == "completed",
        "模型输出未完成（截断或服务错误）"
    );
    let text = value["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["type"] == "message")
        .flat_map(|i| i["content"].as_array().into_iter().flatten())
        .filter(|p| p["type"] == "output_text")
        .filter_map(|p| p["text"].as_str())
        .collect::<String>();
    ensure!(
        !text.trim().is_empty(),
        "模型返回空正文，这不代表录音没有人声"
    );
    Ok(text.trim().into())
}
/// Buffer bytes until SSE event boundaries, preserving split UTF-8 and CRLF.
#[derive(Default)]
struct Sse {
    buffer: Vec<u8>,
    data: Vec<String>,
    text: String,
    total: usize,
    done: bool,
    stopped: bool,
}
impl Sse {
    fn feed(
        &mut self,
        chunk: &[u8],
        responses: bool,
        progress: &mut dyn FnMut(String),
    ) -> Result<()> {
        self.total += chunk.len();
        ensure!(self.total <= MAX_RESPONSE, "流式响应超过 2 MB");
        self.buffer.extend_from_slice(chunk);
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = self.buffer.drain(..=end).collect();
            let line = std::str::from_utf8(&line)
                .context("流式响应不是有效 UTF-8")?
                .trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    let data = self.data.join("\n");
                    self.data.clear();
                    self.event(&data, responses, progress)?;
                    if self.done {
                        break;
                    }
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).into());
            }
        }
        Ok(())
    }
    fn event(
        &mut self,
        data: &str,
        responses: bool,
        progress: &mut dyn FnMut(String),
    ) -> Result<()> {
        if data.trim() == "[DONE]" {
            ensure!(self.stopped, "流式响应缺少成功完成状态");
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(data).context("无法解析流式事件")?;
        ensure!(value.get("error").is_none(), "服务在流式返回过程中报告错误");
        if responses {
            match value["type"].as_str().unwrap_or_default() {
                "response.output_text.delta" => {
                    if let Some(delta) = value["delta"].as_str() {
                        self.text.push_str(delta);
                        progress(self.text.clone());
                    }
                }
                "response.completed" => {
                    self.text = extract_response(&value["response"])?;
                    self.stopped = true;
                    self.done = true;
                }
                "response.failed" | "response.incomplete" | "error" => {
                    bail!("流式模型输出失败或被截断")
                }
                _ => {}
            }
        } else if let Some(choice) = value["choices"].as_array().and_then(|v| v.first()) {
            if let Some(delta) = choice["delta"]["content"].as_str() {
                self.text.push_str(delta);
                progress(self.text.clone());
            }
            if !choice["finish_reason"].is_null() {
                valid_finish(&choice["finish_reason"])?;
                self.stopped = true;
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
}
fn audio_models(value: &Value) -> Result<Vec<Model>> {
    let mut models = value["data"]
        .as_array()
        .context("模型列表格式错误")?
        .iter()
        .filter(|v| {
            v["architecture"]["input_modalities"]
                .as_array()
                .is_some_and(|a| a.iter().any(|m| m == "audio"))
        })
        .filter(|v| {
            v["architecture"]["output_modalities"]
                .as_array()
                .is_some_and(|a| a.iter().any(|m| m == "text"))
        })
        .filter_map(|v| {
            Some(Model {
                id: v["id"].as_str()?.into(),
                name: v["name"].as_str().unwrap_or(v["id"].as_str()?).into(),
            })
        })
        .collect::<Vec<_>>();
    models.sort_by_key(|a| a.name.to_lowercase());
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}
pub fn models(
    client: &Client,
    key: &str,
    cancel: &async_channel::Receiver<()>,
) -> Result<Vec<Model>> {
    ensure!(!key.trim().is_empty(), "请填写 OpenRouter API Key");
    client.run(
        async {
            let response = client
                .http
                .get("https://openrouter.ai/api/v1/models")
                .bearer_auth(key.trim())
                .send()
                .await
                .map_err(network_error)?;
            audio_models(&read_json(response, 12 * 1024 * 1024).await?)
        },
        Some(cancel),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_TRANSCRIPTION_PROMPT, Language};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    fn server(status: &str, kind: &str, body: &str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let task = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let headers = loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                    let len = headers
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|n| n.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + len {
                        break headers;
                    }
                }
            };
            stream.write_all(response.as_bytes()).unwrap();
            let _ = headers;
            String::from_utf8_lossy(&bytes).to_string()
        });
        (format!("http://{address}/v1/chat/completions"), task)
    }
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn provider_payloads_keep_asr_and_multimodal_distinct() {
        let mut c = Config::default();
        c.recognition_mode = RecognitionMode::Asr;
        c.asr_language = Language::Zh;
        c.asr_stream = true;
        let b = transcription_body(&c, b"wav");
        assert_eq!(b["model"], "mimo-v2.5-asr");
        assert_eq!(b["asr_options"]["language"], "zh");
        assert_eq!(b["stream"], true);
        assert!(b.get("thinking").is_none());
        assert_eq!(b["messages"][0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(
            b["messages"][0]["content"][0]["input_audio"]["data"],
            "data:audio/wav;base64,d2F2"
        );
        c.recognition_mode = RecognitionMode::Multimodal;
        let b = transcription_body(&c, b"wav");
        assert_eq!(b["thinking"]["type"], "disabled");
        assert_eq!(
            b["messages"][0]["content"][1]["text"],
            DEFAULT_TRANSCRIPTION_PROMPT
        );
        c.provider = Provider::Openrouter;
        let b = transcription_body(&c, b"wav");
        assert!(b.get("messages").is_none());
        assert!(b.get("thinking").is_none());
        assert_eq!(b["input"][0]["content"][1]["input_audio"]["data"], "d2F2");
    }
    #[test]
    fn http_authentication_and_sse() {
        for mimo in [true, false] {
            let (url, task) = server(
                "200 OK",
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            );
            let mut c = client().unwrap();
            c.http = reqwest::Client::builder().no_proxy().build().unwrap();
            assert_eq!(
                c.run(
                    complete(
                        &c.http,
                        &url,
                        "test-key",
                        mimo,
                        &json!({"stream":true}),
                        false,
                        &mut |_| {}
                    ),
                    None
                )
                .unwrap(),
                "ok"
            );
            let headers = task.join().unwrap().to_lowercase();
            assert!(headers.contains(if mimo {
                "api-key: test-key"
            } else {
                "authorization: bearer test-key"
            }));
            assert!(!headers.contains(if mimo { "authorization:" } else { "api-key:" }));
        }
    }
    #[test]
    fn sse_handles_split_utf8_and_rejects_incomplete_outputs() {
        let data = ": ping\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"你好\",\"reasoning_content\":\"ignore\"},\"finish_reason\":\"stop\"}]}\r\n\r\ndata: [DONE]\r\n\r\n";
        let mut s = Sse::default();
        for b in data.as_bytes() {
            s.feed(&[*b], false, &mut |_| {}).unwrap();
        }
        assert!(s.done);
        assert_eq!(s.text, "你好");
        for data in [
            r#"{"choices":[{"finish_reason":"length"}]}"#,
            r#"{"error":"secret"}"#,
            "[DONE]",
        ] {
            assert!(Sse::default().event(data, false, &mut |_| {}).is_err());
        }
        let mut s = Sse::default();
        s.event(
            r#"{"choices":[{"delta":{"content":"partial"}}]}"#,
            false,
            &mut |_| {},
        )
        .unwrap();
        assert!(!s.done);
    }
    #[test]
    fn responses_ignore_reasoning_and_use_authoritative_final_text() {
        let response = json!({"status":"completed","output":[{"type":"reasoning","content":[{"text":"private"}]},{"type":"message","content":[{"type":"output_text","text":"转写"}]}]});
        assert_eq!(extract_response(&response).unwrap(), "转写");
        let mut s = Sse::default();
        s.event(
            &json!({"type":"response.output_text.delta","delta":"转"}).to_string(),
            true,
            &mut |_| {},
        )
        .unwrap();
        s.event(
            &json!({"type":"response.completed","response":response}).to_string(),
            true,
            &mut |_| {},
        )
        .unwrap();
        assert!(s.done);
        assert_eq!(s.text, "转写");
        assert!(extract_response(&json!({"status":"incomplete"})).is_err());
        assert!(
            extract_chat(
                &json!({"choices":[{"message":{"content":"","reasoning_content":"private"}}]})
            )
            .is_err()
        );
    }
    #[test]
    fn cancellation_interrupts_pending_io_future() {
        let c = client().unwrap();
        let (send, receive) = async_channel::bounded(1);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            send.send_blocking(()).unwrap();
        });
        let start = std::time::Instant::now();
        assert!(
            c.run(
                async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(())
                },
                Some(&receive)
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_closes_a_pending_http_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_send, ready_receive) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = [0; 4096];
            assert!(stream.read(&mut bytes).unwrap() > 0);
            ready_send.send(()).unwrap();
            // No response: only dropping the client request can close this socket.
            while stream.read(&mut bytes).unwrap() > 0 {}
        });
        let (send, receive) = async_channel::bounded(1);
        thread::spawn(move || {
            ready_receive.recv_timeout(Duration::from_secs(2)).unwrap();
            send.send_blocking(()).unwrap();
        });
        let mut c = client().unwrap();
        c.http = reqwest::Client::builder().no_proxy().build().unwrap();
        let result = c.run(
            complete(
                &c.http,
                &format!("http://{address}"),
                "fixture",
                false,
                &json!({}),
                false,
                &mut |_| {},
            ),
            Some(&receive),
        );
        assert!(result.unwrap_err().to_string().contains("取消"));
        // Settings tests own a client per worker; cancellation exits that worker
        // and drops the runtime, including the HTTP/1 connection pool task.
        drop(c);
        server.join().unwrap();
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn polish_uses_its_own_endpoint_key_and_prompt() {
        let (url, task) = server(
            "200 OK",
            "application/json",
            r#"{"choices":[{"finish_reason":"stop","message":{"content":"润色正文"}}]}"#,
        );
        let mut c = client().unwrap();
        c.http = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut config = Config::default();
        config.polish_url = url;
        config.polish_key = "polish-fixture-key".into();
        config.polish_prompt = "polish-fixture-prompt".into();
        config.polish_model = "polish-model".into();
        assert_eq!(polish(&c, &config, "original-text").unwrap(), "润色正文");
        let request = task.join().unwrap();
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer polish-fixture-key")
        );
        assert!(request.contains("polish-fixture-prompt"));
        assert!(request.contains("original-text"));
        assert!(request.contains("polish-model"));
        assert!(!request.contains("input_audio"));
    }
    #[test]
    fn errors_never_echo_credentials() {
        let (url, task) = server(
            "401 Unauthorized",
            "application/json",
            r#"{"error":"sensitive-key"}"#,
        );
        let mut c = client().unwrap();
        c.http = reqwest::Client::builder().no_proxy().build().unwrap();
        let e = c
            .run(
                complete(
                    &c.http,
                    &url,
                    "sensitive-key",
                    false,
                    &json!({}),
                    false,
                    &mut |_| {},
                ),
                None,
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("401"));
        assert!(!e.contains("sensitive-key"));
        task.join().unwrap();
    }
    #[test]
    fn model_filter_requires_audio_in_text_out() {
        let m = |id: &str, i: &str, o: &str| json!({"id":id,"architecture":{"input_modalities":[i],"output_modalities":[o]}});
        let result = audio_models(&json!({"data":[m("audio","audio","text"),m("text","text","text"),m("tts","text","audio")]})).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "audio");
    }
    #[test]
    fn compatible_url_accepts_base_and_full_endpoint() {
        for url in [
            "http://localhost/v1",
            "http://localhost/v1/",
            "http://localhost/v1/chat",
            "http://localhost/v1/chat/completions",
        ] {
            assert_eq!(
                completion_url(url).unwrap(),
                "http://localhost/v1/chat/completions"
            );
        }
        assert!(completion_url("https://key:secret@example.com").is_err());
        assert!(completion_url("file:///tmp").is_err());
    }
}
