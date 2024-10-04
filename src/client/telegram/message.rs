/*
:project: telegram-onedrive
:author: L-ING
:copyright: (C) 2024 L-ING <hlf01@icloud.com>
:license: MIT, see LICENSE for more details.
*/

use super::TelegramClient;
use crate::{
    error::{Error, Result, ResultExt},
    message::{ChatEntity, QueuedMessage, QueuedMessageType, TelegramMessage},
    trace::indenter,
};
use grammers_client::{
    client::messages::MessageIter,
    types::{Chat, InputMessage, PackedChat},
    Update,
};
use proc_macros::{add_context, add_trace};
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};
use tokio::sync::mpsc;

impl TelegramClient {
    #[add_context]
    #[add_trace]
    pub async fn get_message<C>(&self, chat: C, message_id: i32) -> Result<TelegramMessage>
    where
        C: Into<PackedChat>,
    {
        let message_raw = self
            .raw()
            .get_messages_by_id(chat, &[message_id])
            .await
            .map_err(|e| Error::new("failed to get message by id").raw(e))?
            .first()
            .ok_or_else(|| Error::new("message vec is empty"))?
            .to_owned()
            .ok_or_else(|| Error::new("message not found"))?;

        let message = TelegramMessage::new(self.clone(), message_raw);

        tracing::debug!("got message {} in chat {}", message_id, message.chat().id());

        Ok(message)
    }

    #[add_context]
    #[add_trace]
    pub async fn get_chat(&self, chat_entity: &ChatEntity) -> Result<Chat> {
        let mut dialogs = self.raw().iter_dialogs();

        while let Some(dialog) = dialogs
            .next()
            .await
            .map_err(|e| Error::new("failed to get dialog").raw(e))?
        {
            let chat = dialog.chat();

            if match chat_entity {
                ChatEntity::Chat(chat_old) => chat.id() == chat_old.id(),
                ChatEntity::Id(chat_id) => chat.id() == *chat_id,
                ChatEntity::Username(chat_username) => chat.username().map_or_else(
                    || chat.usernames().contains(&chat_username.as_str()),
                    |username| username == chat_username,
                ),
            } {
                tracing::debug!("got chat {}", chat.id());

                return Ok(chat.to_owned());
            }
        }

        Err(Error::new("chat not found"))
    }

    #[add_trace]
    pub fn iter_messages<C: Into<PackedChat>>(&self, chat: C) -> MessageIter {
        self.raw().iter_messages(chat)
    }

    #[add_context]
    #[add_trace]
    pub async fn delete_messages<C: Into<PackedChat>>(
        &self,
        chat: C,
        message_ids: &[i32],
    ) -> Result<usize> {
        self.raw()
            .delete_messages(chat, message_ids)
            .await
            .map_err(|e| Error::new("failed to delete messages").raw(e))
    }

    #[add_context]
    #[add_trace]
    pub async fn send_message<C: Into<PackedChat>, M: Into<InputMessage>>(
        &self,
        chat: C,
        message: M,
    ) -> Result<TelegramMessage> {
        let (tx, mut rx) = mpsc::channel(1);

        let queued_message = QueuedMessage::new(QueuedMessageType::Respond, message, chat, tx);

        self.push_queued_message(queued_message).await;

        rx.recv()
            .await
            .ok_or_else(|| Error::new("failed to receive message result"))??
            .ok_or_else(|| Error::new("received message is None"))
    }

    #[add_context]
    #[add_trace]
    pub async fn edit_message<C: Into<PackedChat>, M: Into<InputMessage>>(
        &self,
        chat: C,
        message_id: i32,
        new_message: M,
    ) -> Result<()> {
        let (tx, mut rx) = mpsc::channel(1);

        let queued_message =
            QueuedMessage::new(QueuedMessageType::Edit(message_id), new_message, chat, tx);

        self.push_queued_message(queued_message).await;

        if let Some(result) = rx.recv().await {
            result?;
        }

        Ok(())
    }

    #[add_context]
    #[add_trace]
    pub async fn next_update(&self) -> Result<Update> {
        self.raw()
            .next_update()
            .await
            .map_err(|e| Error::new("Failed to get next update").raw(e))
    }

    #[add_trace]
    pub async fn push_queued_message(&self, queued_message: QueuedMessage) {
        self.chat_message_queue()
            .lock()
            .await
            .push_back(queued_message);
    }

    #[add_trace]
    pub fn run_message_loop(&self) {
        let chat_message_queue = self.chat_message_queue();
        let telegram_client = self.clone();

        let mut rng = {
            let rng = rand::thread_rng();
            StdRng::from_rng(rng)
                .map_err(|e| Error::new("failed to create rng").raw(e))
                .unwrap_or_trace()
        };

        tokio::spawn(async move {
            indenter::set_file_indenter(indenter::Coroutine::Message, async {
                loop {
                    let mut chat_message_queue = chat_message_queue.lock().await;

                    let chat_ids = chat_message_queue.keys().copied().collect::<Vec<i64>>();
                    for chat_id in chat_ids {
                        if let Some(QueuedMessage {
                            message_type,
                            input_message,
                            chat,
                            tx,
                        }) = chat_message_queue.pop_front(chat_id)
                        {
                            let message_result = match message_type {
                                QueuedMessageType::Respond => {
                                    let result = telegram_client
                                        .raw()
                                        .send_message(chat, input_message)
                                        .await
                                        .map_err(|e| {
                                            Error::new("failed to respond message").raw(e)
                                        });

                                    match result {
                                        Ok(message_raw) => Ok(Some(TelegramMessage::new(
                                            telegram_client.clone(),
                                            message_raw,
                                        ))),
                                        Err(e) => Err(e),
                                    }
                                }
                                QueuedMessageType::Reply(message_id) => {
                                    let result = telegram_client
                                        .raw()
                                        .send_message(
                                            chat,
                                            input_message.reply_to(Some(message_id)),
                                        )
                                        .await
                                        .map_err(|e| {
                                            Error::new("failed to respond message").raw(e)
                                        });

                                    match result {
                                        Ok(message_raw) => Ok(Some(TelegramMessage::new(
                                            telegram_client.clone(),
                                            message_raw,
                                        ))),
                                        Err(e) => Err(e),
                                    }
                                }
                                QueuedMessageType::Edit(message_id) => {
                                    let result = telegram_client
                                        .raw()
                                        .edit_message(chat, message_id, input_message)
                                        .await
                                        .map_err(|e| {
                                            Error::new("failed to respond message").raw(e)
                                        });

                                    match result {
                                        Ok(()) => Ok(None),
                                        Err(e) => Err(e),
                                    }
                                }
                            };

                            tx.send(message_result)
                                .await
                                .map_err(|e| {
                                    Error::new("failed to send message result to rx").raw(e)
                                })
                                .trace();
                        }

                        if chat_message_queue.get(&chat_id).unwrap().is_empty() {
                            chat_message_queue.remove(&chat_id);
                        }
                    }

                    drop(chat_message_queue);

                    let millis = rng.gen_range(1500..4000);
                    tokio::time::sleep(Duration::from_millis(millis)).await;
                }
            })
            .await;
        });
    }
}

pub struct MessageVecDeque {
    deque: VecDeque<QueuedMessage>,
    // message id -> index in deque, only for edit messages
    key_map: HashMap<i32, usize>,
}

impl MessageVecDeque {
    pub fn new() -> Self {
        Self {
            deque: VecDeque::new(),
            key_map: HashMap::new(),
        }
    }

    fn push_back(&mut self, queued_message: QueuedMessage) {
        match queued_message.message_type {
            QueuedMessageType::Respond | QueuedMessageType::Reply(_) => {
                self.deque.push_back(queued_message);
            }
            QueuedMessageType::Edit(message_id) => {
                if let Some(index) = self.key_map.get(&message_id) {
                    // override the outdated edit message
                    self.deque[*index] = queued_message;
                } else {
                    self.key_map.insert(message_id, self.deque.len());
                    self.deque.push_back(queued_message);
                }
            }
        }
    }

    fn pop_front(&mut self) -> Option<QueuedMessage> {
        self.deque.pop_front().map(|queued_message| {
            if let QueuedMessageType::Edit(message_id) = queued_message.message_type {
                self.key_map.remove(&message_id);
            }

            // decrease the index of all edit messages
            for index in self.key_map.values_mut() {
                *index -= 1;
            }

            queued_message
        })
    }

    fn is_empty(&self) -> bool {
        self.deque.is_empty()
    }
}

pub type ChatMessageVecDeque = HashMap<i64, MessageVecDeque>;

trait ChatMessageHashMapExt {
    fn push_back(&mut self, queued_message: QueuedMessage);

    fn pop_front(&mut self, chat_id: i64) -> Option<QueuedMessage>;
}

impl ChatMessageHashMapExt for ChatMessageVecDeque {
    #[add_trace]
    fn push_back(&mut self, queued_message: QueuedMessage) {
        self.entry(queued_message.chat.id)
            .or_insert_with(MessageVecDeque::new)
            .push_back(queued_message);
    }

    fn pop_front(&mut self, chat_id: i64) -> Option<QueuedMessage> {
        self.get_mut(&chat_id)?.pop_front()
    }
}
