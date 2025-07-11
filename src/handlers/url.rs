/*
:project: telegram-onedrive
:author: L-ING
:copyright: (C) 2024 L-ING <hlf01@icloud.com>
:license: MIT, see LICENSE for more details.
*/

use std::sync::atomic::Ordering;

use super::{
    docs::{format_help, format_unknown_command_help},
    utils::{
        get_filename,
        text::{cmd_parser, TextExt},
    },
};
use crate::{
    handlers::utils::message::format_message_link,
    message::{ChatEntity, TelegramMessage},
    state::AppState,
    tasker::{CmdType, InsertTask},
    utils::get_http_client,
};
use anyhow::{anyhow, Context, Result};
use grammers_client::InputMessage;
use proc_macros::{check_in_group, check_od_login, check_senders, check_tg_login};
use reqwest::header;

pub const PATTERN: &str = "/url";

#[check_od_login]
#[check_tg_login]
#[check_senders]
#[check_in_group]
pub async fn handler(message: TelegramMessage, state: AppState) -> Result<()> {
    let cmd = cmd_parser(message.text());

    if cmd.len() == 2 {
        if cmd[1] == "help" {
            // /url help
            message
                .respond(InputMessage::html(format_help(PATTERN)))
                .await
                .context("help")?;

            Ok(())
        } else {
            // /url $url
            let telegram_user = &state.telegram_user;
            let onedrive = &state.onedrive;
            let task_session = &state.task_session;

            // 解析是否重命名
            let (raw_url, file_rename) = match cmd[1].split_once('|') {
                Some((url_part, name_part)) => (url_part.trim(), Some(name_part.trim())),
                None => (cmd[1].trim(), None),
            };

            let url = raw_url.url_encode();

            if url.starts_with("http://") || url.starts_with("https://") {
                let http_client = get_http_client()?;

                let response = http_client
                    .head(&url)
                    .send()
                    .await
                    .context("failed to send head request for /url")?;

                // 获取默认文件名（含扩展名）
                let default_filename = get_filename(
                    response.url().as_ref(),
                    &response,
                    &onedrive.get_root_path(false).await?,
                )?;

                // 获取扩展名（包含 .）
                let extension = match std::path::Path::new(&default_filename).extension() {
                    Some(ext) => format!(".{}", ext.to_string_lossy()),
                    None => "".to_string(),
                };

                // 使用自定义文件名 + 保留的扩展名（如果有）
                let filename = if let Some(name) = file_rename {
                    format!("{}{}", name, extension)
                } else {
                    default_filename
                };

                let total_length = match response.headers().get(header::CONTENT_LENGTH) {
                    Some(content_length) => content_length
                        .to_str()
                        .context("header Content-Length has invisible ASCII chars")?
                        .parse::<u64>()
                        .context("failed to parse header Content-Length to u64")?,
                    None => return Err(anyhow!(
                        "Content-Length not found in response headers.\nStatus code:\n{}\nResponse headers:\n{:#?}",
                        response.status(),
                        response.headers()
                    )),
                };

                let chat_user = telegram_user
                    .get_chat(&ChatEntity::from(message.chat()))
                    .await?;

                let response = format!(
                    "{}\n\n{}",
                    url,
                    format_message_link(chat_user.id(), message.id(), &filename)
                );
                let message_indicator_id = message
                    .respond(InputMessage::html(&response))
                    .await
                    .context(response)?
                    .id();

                let root_path = onedrive.get_root_path(true).await?;

                let (upload_session, upload_session_meta) = onedrive
                    .multipart_upload_session_builder(&root_path, &filename)
                    .await?;

                let current_length = upload_session_meta
                    .next_expected_ranges
                    .first()
                    .map_or(0, |range| range.start);

                let chat_bot_hex = message.chat().pack().to_hex();
                let chat_user_hex = chat_user.pack().to_hex();

                let auto_delete = state.should_auto_delete.load(Ordering::Acquire);

                // in case if cancellation happens before inserting the task
                let _aborters = state.task_session.task_aborters.lock().await;

                task_session
                    .insert_task(InsertTask {
                        cmd_type: CmdType::Url,
                        filename: filename.clone(),
                        root_path,
                        url: Some(url),
                        upload_url: upload_session.upload_url().to_string(),
                        current_length,
                        total_length,
                        chat_id: message.chat().id(),
                        chat_bot_hex,
                        chat_user_hex,
                        chat_origin_hex: None,
                        message_id: message.id(),
                        message_indicator_id,
                        message_origin_id: None,
                        auto_delete,
                    })
                    .await?;

                tracing::info!("inserted url task: {} size: {}", filename, total_length);

                Ok(())
            } else {
                Err(anyhow!("not an http url"))
            }
        }
    } else {
        Err(anyhow!(format_unknown_command_help(PATTERN)))
    }
}
