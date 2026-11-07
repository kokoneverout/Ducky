use crate::model::{Settings, hash, key};
use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use reqwest::{Client, Response, Url};
use serde::{Deserialize, Serialize};
use std::time::Duration;
#[derive(Clone, Debug)]
pub struct Remote {
    pub signature: String,
    pub text: Option<String>,
}
#[derive(Serialize, Deserialize, Debug)]
pub struct Profile {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    #[serde(rename = "hasData")]
    pub has_data: bool,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default, rename = "dataName")]
    pub data_name: Option<String>,
    #[serde(default, rename = "transferDataHash")]
    pub transfer_hash: Option<String>,
}
pub fn base_url(input: &str) -> Result<Url> {
    let mut u = Url::parse(input.trim()).context("服务器地址无效")?;
    ensure!(
        matches!(u.scheme(), "http" | "https")
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none(),
        "地址需为 http(s)，不能内嵌凭据、查询或片段"
    );
    if !u.path().ends_with('/') {
        u.set_path(&format!("{}/", u.path()));
    }
    Ok(u)
}
fn endpoint(s: &Settings, path: &str) -> Result<Url> {
    Ok(base_url(&s.server_url)?.join(path)?)
}
pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}
async fn bytes(response: Response, max: usize) -> Result<Vec<u8>> {
    let status = response.status();
    ensure!(
        status.is_success(),
        "HTTP {}：{}",
        status.as_u16(),
        match status.as_u16() {
            401 => "账号或密码错误",
            403 => "服务端拒绝访问",
            404 => "接口或文本文件不存在",
            429 => "请求过于频繁",
            _ => "服务端请求失败",
        }
    );
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(
            out.len().saturating_add(chunk.len()) <= max,
            "远端内容超过大小上限"
        );
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c.is_control() || "/\\:%?#".contains(c))
}
pub fn verify_text(text: &str, expected: Option<&str>, limit: usize) -> Result<()> {
    ensure!(text.len() <= limit, "远端正文超过同步上限");
    if let Some(h) = expected.filter(|h| !h.is_empty()) {
        ensure!(hash(text).eq_ignore_ascii_case(h), "文本 SHA-256 校验失败");
    }
    Ok(())
}
pub async fn fetch(c: &Client, s: &Settings, password: &str) -> Result<Remote> {
    let body = bytes(
        c.get(endpoint(s, "SyncClipboard.json")?)
            .basic_auth(&s.username, Some(password))
            .header("Cache-Control", "no-cache")
            .send()
            .await?,
        s.max_sync_bytes.saturating_mul(6) + 65536,
    )
    .await?;
    let body = body.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&body);
    // Non-text records need not obey Text's field requirements.
    let value: serde_json::Value = serde_json::from_slice(body)?;
    let kind = value
        .get("type")
        .and_then(|v| v.as_str())
        .context("协议缺少小写 type 字段")?;
    if kind != "Text" {
        return Ok(Remote {
            signature: format!(
                "{}:{}",
                kind,
                value
                    .get("hash")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&String::from_utf8_lossy(body))
            ),
            text: None,
        });
    }
    let profile: Profile = serde_json::from_value(value)?;
    let text = if profile.has_data {
        let name = profile.data_name.as_deref().context("缺少 dataName")?;
        ensure!(valid_name(name), "远端文本文件名无效");
        let mut url = endpoint(s, "file/")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("文件地址无效"))?
            .pop_if_empty()
            .push(name);
        let body = bytes(
            c.get(url)
                .basic_auth(&s.username, Some(password))
                .send()
                .await?,
            s.max_sync_bytes + 3,
        )
        .await?;
        if let Some(h) = profile.transfer_hash.as_deref().filter(|h| !h.is_empty()) {
            use sha2::{Digest, Sha256};
            ensure!(
                format!("{:X}", Sha256::digest(&body)).eq_ignore_ascii_case(h),
                "传输文件 SHA-256 校验失败"
            );
        }
        String::from_utf8(
            body.strip_prefix(&[0xef, 0xbb, 0xbf])
                .unwrap_or(&body)
                .to_vec(),
        )?
    } else {
        profile.text
    };
    verify_text(&text, profile.hash.as_deref(), s.max_sync_bytes)?;
    Ok(Remote {
        signature: format!("Text:{}", key(&text)),
        text: Some(text),
    })
}
pub async fn upload(c: &Client, s: &Settings, password: &str, text: &str) -> Result<()> {
    ensure!(text.len() <= s.max_sync_bytes, "文本超过同步上限");
    let p = Profile {
        kind: "Text".into(),
        text: text.into(),
        has_data: false,
        hash: Some(hash(text)),
        data_name: None,
        transfer_hash: None,
    };
    let response = c
        .put(endpoint(s, "SyncClipboard.json")?)
        .basic_auth(&s.username, Some(password))
        .json(&p)
        .send()
        .await?;
    bytes(response, 65536).await?;
    Ok(())
}
