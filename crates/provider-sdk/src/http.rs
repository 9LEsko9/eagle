// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! HTTP access for providers. The host implements [`Http`], so providers share its
//! retries, limits, proxy settings and worker pool instead of creating clients.

use crate::{SignedUrl, Token};

#[derive(Clone, Copy)]
pub enum Priority {
    Playback = 0,
    Preload = 1,
    Metadata = 2,
    Background = 3,
}
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("ETag или размер файла изменился; повторите воспроизведение")]
    Changed,
    #[error("{0}")]
    Source(&'static str),
    #[error("HTTP {0}")]
    Status(u16),
    #[error("ошибка сети или таймаут")]
    Network,
    #[error("ответ сервера слишком большой")]
    TooLarge,
    #[error("сетевой агент остановлен")]
    Stopped,
    #[error("очередь сети заполнена")]
    QueueFull,
}
pub struct Request {
    pub url: SignedUrl,
    pub method: &'static str,
    pub headers: Vec<(String, String)>,
    pub token: Option<Token>,
    pub form: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub range: Option<(u64, u64)>,
    pub limit: usize,
}
impl Request {
    pub fn get(url: SignedUrl) -> Self {
        Self {
            url,
            method: "GET",
            headers: vec![],
            token: None,
            form: vec![],
            body: None,
            range: None,
            limit: 16 * 1024 * 1024,
        }
    }
}
pub struct Response {
    pub status: u16,
    pub bytes: Vec<u8>,
    pub content_range: Option<String>,
    pub etag: Option<String>,
}
/// Blocking request/response access; the host schedules it by `Priority`.
pub trait Http: Send + Sync {
    /// `refresh` is asked for a new token once if the server answers 401.
    fn request(
        &self,
        priority: Priority,
        request: Request,
        refresh: Option<&(dyn Fn() -> Result<Token, HttpError> + Sync)>,
    ) -> Result<Response, HttpError>;
}
