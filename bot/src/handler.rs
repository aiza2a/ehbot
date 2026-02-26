use std::{borrow::Cow, collections::HashSet, sync::Arc};
use std::collections::HashMap;
use std::sync::Mutex;

use eh2telegraph::{
    collector::{e_hentai::EHCollector, exhentai::EXCollector, nhentai::NHCollector, AlbumMeta, ImageMeta, ImageData},
    config::{self, WhitelistConfig},
    searcher::{
        f_hash::FHashConvertor,
        saucenao::{SaucenaoOutput, SaucenaoParsed, SaucenaoSearcher},
        ImageSearcher,
    },
    storage::KVStorage,
    sync::Synchronizer,
};

use reqwest::Url;
use teloxide::{
    adaptors::DefaultParseMode,
    prelude::*,
    types::{InputMedia, InputMediaPhoto, InputFile, ParseMode, MessageId},
    utils::{
        command::BotCommands,
        markdown::{code_inline, escape, link},
    },
};
use tokio::sync::oneshot;
use tracing::{info, trace};

use crate::{ok_or_break, util::PrettyChat};

const MIN_SIMILARITY: u8 = 70;
const MIN_SIMILARITY_PRIVATE: u8 = 50;

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "\
    This is a gallery synchronization robot that is convenient for users to view pictures directly in Telegram.\n\
    Bot supports sync with command, text url, or image(private chat search thrashold is lower).\n\
    [New] Support range sync: /sync <url> <start> <end> (e.g., /sync url 3 16 or url/3)\n\n\
    These commands are supported:"
)]
pub enum Command {
    #[command(description = "Display this help. 顯示這條幫助信息")]
    Help,
    #[command(description = "Show bot verison. 顯示機器人版本")]
    Version,
    #[command(description = "Show your account id. 顯示你的帳號 ID")]
    Id,
    #[command(description = "Sync a gallery. 同步一個畫廊(目前支持 EH/EX/NH)")]
    Sync(String),
    #[command(description = "Cancel all ongoing sync operations. 取消所有正在進行的同步操作。")]
    Cancel,
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Command for admins")]
pub enum AdminCommand {
    #[command(description = "Delete cache with given key.")]
    Delete(String),
}

pub struct Handler<C> {
    pub synchronizer: Synchronizer<C>,
    pub searcher: SaucenaoSearcher,
    pub convertor: FHashConvertor,
    pub admins: HashSet<i64>,
    pub whitelist: HashSet<i64>,

    single_flight: singleflight_async::SingleFlight<String>,
    active_syncs: Arc<Mutex<HashMap<i64, Vec<(String, oneshot::Sender<()>)>>>>,
}

impl<C> Handler<C>
where
    C: KVStorage<String> + Send + Sync + 'static,
{
    pub fn new(synchronizer: Synchronizer<C>, admins: HashSet<i64>) -> Self {
        let whitelist = match config::parse::<WhitelistConfig>("whitelist")
            .ok()
            .and_then(|x| x)
        {
            Some(config) => {
                if config.enabled { config.ids.into_iter().collect() } 
                else { HashSet::from([i64::MIN]) }
            }
            None => HashSet::from([i64::MIN]),
        };

        Self {
            synchronizer,
            searcher: SaucenaoSearcher::new_from_config(),
            convertor: FHashConvertor::new_from_config(),
            admins,
            whitelist,
            single_flight: Default::default(),
            active_syncs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn is_allowed(&self, chat_id: i64) -> bool {
        if self.admins.contains(&chat_id) || self.whitelist.contains(&i64::MIN) {
            return true;
        }
        self.whitelist.contains(&chat_id)
    }

    fn register_sync(&self, user_id: i64, url: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let mut active_syncs = self.active_syncs.lock().unwrap();
        let user_syncs = active_syncs.entry(user_id).or_insert_with(Vec::new);
        user_syncs.push((url.to_string(), tx));
        rx
    }

    fn unregister_sync(&self, user_id: i64, url: &str) {
        let mut active_syncs = self.active_syncs.lock().unwrap();
        if let Some(user_syncs) = active_syncs.get_mut(&user_id) {
            user_syncs.retain(|(sync_url, _)| sync_url != url);
            if user_syncs.is_empty() {
                active_syncs.remove(&user_id);
            }
        }
    }

    fn cancel_all_syncs(&self, user_id: i64) -> usize {
        let mut active_syncs = self.active_syncs.lock().unwrap();
        if let Some(user_syncs) = active_syncs.remove(&user_id) {
            let count = user_syncs.len();
            for (_, tx) in user_syncs {
                let _ = tx.send(());
            }
            count
        } else {
            0
        }
    }

    async fn send_unauthorized(&self, bot: &DefaultParseMode<Bot>, msg: &Message) {
        if msg.chat.is_private() {
            let _ = bot.send_message(msg.chat.id, escape("User not authorized!")).reply_to_message_id(msg.id).await;
        }
    }

    pub async fn respond_cmd(
        &'static self,
        bot: DefaultParseMode<Bot>,
        msg: Message,
        command: Command,
    ) -> ControlFlow<()> {
        match command {
            Command::Help => { let _ = bot.send_message(msg.chat.id, escape(&Command::descriptions().to_string())).reply_to_message_id(msg.id).await; }
            Command::Version => { let _ = bot.send_message(msg.chat.id, escape(crate::version::VERSION)).reply_to_message_id(msg.id).await; }
            Command::Id => { let _ = bot.send_message(msg.chat.id, format!("Current chat id is {}", code_inline(&msg.chat.id.to_string()))).reply_to_message_id(msg.id).await; }
            Command::Cancel => {
                let count = self.cancel_all_syncs(msg.chat.id.0);
                let text = if count > 0 { format!("Cancelled {} sync operations.", count) } else { "No active sync operations.".to_string() };
                let _ = bot.send_message(msg.chat.id, escape(&text)).reply_to_message_id(msg.id).await;
            }
            Command::Sync(input) => {
                if !self.is_allowed(msg.chat.id.0) {
                    self.send_unauthorized(&bot, &msg).await;
                    return ControlFlow::Break(());
                }
                if input.is_empty() {
                    let _ = bot.send_message(msg.chat.id, escape("Usage: /sync url [start] [end]")).reply_to_message_id(msg.id).await;
                    return ControlFlow::Break(());
                }

                let parts: Vec<&str> = input.split_whitespace().collect();
                let mut raw_url = parts.first().unwrap_or(&"").to_string();
                let mut start_page: Option<usize> = None;
                let mut end_page: Option<usize> = None;

                if let Ok(mut parsed_url) = Url::parse(&raw_url) {
                    if let Some(last_seg) = parsed_url.path_segments().and_then(|s| s.last()) {
                        if let Ok(page) = last_seg.parse::<usize>() {
                            start_page = Some(page);
                            end_page = Some(page);
                            parsed_url.path_segments_mut().unwrap().pop();
                            raw_url = parsed_url.to_string();
                        }
                    }
                }

                if parts.len() == 2 {
                    if let Ok(p) = parts[1].parse::<usize>() {
                        start_page = Some(p);
                        end_page = Some(p);
                    }
                } else if parts.len() >= 3 {
                    start_page = parts[1].parse::<usize>().ok();
                    end_page = parts[2].parse::<usize>().ok();
                }

                if let (Some(s), Some(e)) = (start_page, end_page) {
                    if s > e {
                        start_page = Some(e);
                        end_page = Some(s);
                    }
                }

                info!("[cmd handler] receive sync request from {:?} for {raw_url}", PrettyChat(&msg.chat));
                
                let original_msg_id = msg.id;
                let prompt_msg: Message = ok_or_break!(bot.send_message(msg.chat.id, escape(&format!("Syncing url {raw_url}"))).reply_to_message_id(original_msg_id).await);
                let prompt_msg_id = prompt_msg.id;

                let url_clone = raw_url.clone();
                let cancel_rx = self.register_sync(msg.chat.id.0, &raw_url);

                let diff = match (start_page, end_page) {
                    (Some(s), Some(e)) => e - s,
                    _ => usize::MAX, 
                };

                if diff <= 5 {
                    let s = start_page.unwrap();
                    let e = end_page.unwrap();
                    tokio::spawn(async move {
                        self.send_media_group_response(&bot, msg.chat.id, prompt_msg_id, original_msg_id, &url_clone, s, e, cancel_rx).await;
                        self.unregister_sync(msg.chat.id.0, &url_clone);
                    });
                } else {
                    tokio::spawn(async move {
                        let result = self.sync_range_response(&url_clone, start_page, end_page, cancel_rx).await;
                        self.unregister_sync(msg.chat.id.0, &url_clone);
                        let _ = bot.edit_message_text(msg.chat.id, prompt_msg_id, result).await;
                    });
                }
            }
        };
        ControlFlow::Break(())
    }

    pub async fn respond_admin_cmd(&'static self, bot: DefaultParseMode<Bot>, msg: Message, command: AdminCommand) -> ControlFlow<()> {
        if let AdminCommand::Delete(key) = command {
            tokio::spawn(async move {
                let _ = self.synchronizer.delete_cache(&key).await;
                let _ = bot.send_message(msg.chat.id, escape(&format!("Key {key} deleted."))).reply_to_message_id(msg.id).await;
            });
        }
        ControlFlow::Break(())
    }

    pub async fn respond_text(&'static self, bot: DefaultParseMode<Bot>, msg: Message) -> ControlFlow<()> {
        if !self.is_allowed(msg.chat.id.0) {
            self.send_unauthorized(&bot, &msg).await;
            return ControlFlow::Break(());
        }
        let maybe_link = {
            let entries = msg.entities().map(|es| es.iter().filter_map(|e| if let teloxide::types::MessageEntityKind::TextLink { url } = &e.kind { Synchronizer::match_url_from_text(url.as_ref()).map(ToOwned::to_owned) } else { None })).into_iter().flatten();
            msg.text().and_then(|content| Synchronizer::match_url_from_text(content).map(ToOwned::to_owned)).into_iter().chain(entries).next()
        };

        if let Some(url) = maybe_link {
            let original_msg_id = msg.id;
            let prompt_msg: Message = ok_or_break!(bot.send_message(msg.chat.id, escape(&format!("Syncing url {url}"))).reply_to_message_id(original_msg_id).await);
            
            let url_clone = url.clone();
            let cancel_rx = self.register_sync(msg.chat.id.0, &url);
            tokio::spawn(async move {
                let result = self.sync_range_response(&url, None, None, cancel_rx).await;
                self.unregister_sync(msg.chat.id.0, &url_clone);
                let _ = bot.edit_message_text(msg.chat.id, prompt_msg.id, result).await;
            });
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    pub async fn respond_caption(&'static self, bot: DefaultParseMode<Bot>, msg: Message) -> ControlFlow<()> {
        if !self.is_allowed(msg.chat.id.0) {
            self.send_unauthorized(&bot, &msg).await;
            return ControlFlow::Break(());
        }
        let caption_entities = msg.caption_entities();
        let mut final_url = None;
        for entry in caption_entities.map(|x| x.iter()).into_iter().flatten() {
            let url = match &entry.kind {
                teloxide::types::MessageEntityKind::Url => {
                    let raw = msg.caption().expect("Url MessageEntry found but caption is None");
                    let encoded: Vec<_> = raw.encode_utf16().skip(entry.offset).take(entry.length).collect();
                    let content = ok_or_break!(String::from_utf16(&encoded));
                    Cow::from(content)
                }
                teloxide::types::MessageEntityKind::TextLink { url } => Cow::from(url.as_ref()),
                _ => continue,
            };
            if let Some(c) = Synchronizer::match_url_from_url(&url) {
                final_url = Some(c.to_string());
                break;
            }
        }

        if let Some(url) = final_url {
            let original_msg_id = msg.id;
            let prompt_msg: Message = ok_or_break!(bot.send_message(msg.chat.id, escape(&format!("Syncing url {url}"))).reply_to_message_id(original_msg_id).await);
            
            let url_clone = url.clone();
            let cancel_rx = self.register_sync(msg.chat.id.0, &url);
            tokio::spawn(async move {
                let result = self.sync_range_response(&url, None, None, cancel_rx).await;
                self.unregister_sync(msg.chat.id.0, &url_clone);
                let _ = bot.edit_message_text(msg.chat.id, prompt_msg.id, result).await;
            });
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    pub async fn respond_photo(&'static self, bot: DefaultParseMode<Bot>, msg: Message) -> ControlFlow<()> {
        if !self.is_allowed(msg.chat.id.0) {
            self.send_unauthorized(&bot, &msg).await;
            return ControlFlow::Break(());
        }
        let first_photo = match msg.photo().and_then(|x| x.first()) {
            Some(p) => p,
            None => return ControlFlow::Continue(()),
        };

        let f = ok_or_break!(bot.get_file(&first_photo.file.id).await);
        let mut buf: Vec<u8> = Vec::with_capacity(f.size as usize);
        ok_or_break!(teloxide::net::Download::download_file(&bot, &f.path, &mut buf).await);
        let search_result: SaucenaoOutput = ok_or_break!(self.searcher.search(buf).await);

        let mut url_sim = None;
        let threshold = if msg.chat.is_private() { MIN_SIMILARITY_PRIVATE } else { MIN_SIMILARITY };
        
        for element in search_result.data.into_iter().filter(|x| x.similarity >= threshold) {
            match element.parsed {
                SaucenaoParsed::EHentai(f_hash) => {
                    url_sim = Some((ok_or_break!(self.convertor.convert_to_gallery(&f_hash).await), element.similarity));
                    break;
                }
                SaucenaoParsed::NHentai(nid) => {
                    url_sim = Some((format!("https://nhentai.net/g/{nid}/"), element.similarity));
                    break;
                }
                _ => continue,
            }
        }

        if let Some((url, _)) = url_sim {
            let original_msg_id = msg.id;
            if let Ok(prompt_msg) = bot.send_message(msg.chat.id, escape(&format!("Syncing url {url}"))).reply_to_message_id(original_msg_id).await {
                let url_clone = url.clone();
                let cancel_rx = self.register_sync(msg.chat.id.0, &url);
                tokio::spawn(async move {
                    let result = self.sync_range_response(&url, None, None, cancel_rx).await;
                    self.unregister_sync(msg.chat.id.0, &url_clone);
                    let _ = bot.edit_message_text(msg.chat.id, prompt_msg.id, result).await;
                });
            }
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    pub async fn respond_default(&'static self, bot: DefaultParseMode<Bot>, msg: Message) -> ControlFlow<()> {
        if msg.chat.is_private() {
            ok_or_break!(bot.send_message(msg.chat.id, escape("Unrecognized message. Maybe /help ?")).reply_to_message_id(msg.id).await);
        }
        ControlFlow::Break(())
    }

    // ==== 核心路由與發送方法 ====

    async fn sync_range_response(&self, url: &str, start: Option<usize>, end: Option<usize>, mut cancel_rx: oneshot::Receiver<()>) -> String {
        tokio::select! {
            result = self.single_flight.work(url, || async {
                match self.route_sync(url, start, end).await {
                    Ok(sync_url) => format!("Sync to telegraph finished: {}", link(&sync_url, &escape(&sync_url))),
                    Err(e) => format!("Sync to telegraph failed: {}", escape(&e.to_string())),
                }
            }) => result,
            _ = &mut cancel_rx => "Sync operation was cancelled.".to_string()
        }
    }

    async fn route_sync(&self, url: &str, start: Option<usize>, end: Option<usize>) -> anyhow::Result<String> {
        let u = Url::parse(url).map_err(|_| anyhow::anyhow!("Invalid url"))?;
        let host = u.host_str().unwrap_or_default();
        let path = u.path().to_string();

        match host {
            "e-hentai.org" => self.synchronizer.sync::<EHCollector>(path, start, end).await.map_err(Into::into),
            "nhentai.to" | "nhentai.net" => self.synchronizer.sync::<NHCollector>(path, start, end).await.map_err(Into::into),
            "exhentai.org" => self.synchronizer.sync::<EXCollector>(path, start, end).await.map_err(Into::into),
            _ => Err(anyhow::anyhow!("no matching collector")),
        }
    }

    async fn route_fetch_images(&self, url: &str, start: usize, end: usize) -> anyhow::Result<(AlbumMeta, Vec<(ImageMeta, ImageData)>)> {
        let u = Url::parse(url).map_err(|_| anyhow::anyhow!("Invalid url"))?;
        let host = u.host_str().unwrap_or_default();
        let path = u.path().to_string();

        match host {
            "e-hentai.org" => self.synchronizer.fetch_images_only::<EHCollector>(path, start, end).await,
            "exhentai.org" => self.synchronizer.fetch_images_only::<EXCollector>(path, start, end).await,
            "nhentai.to" | "nhentai.net" => self.synchronizer.fetch_images_only::<NHCollector>(path, start, end).await,
            _ => Err(anyhow::anyhow!("no matching collector")),
        }
    }

    async fn send_media_group_response(
        &self,
        bot: &DefaultParseMode<Bot>,
        chat_id: ChatId,
        prompt_msg_id: MessageId,
        reply_msg_id: MessageId,
        url: &str,
        start: usize,
        end: usize,
        mut cancel_rx: oneshot::Receiver<()>,
    ) {
        let url_clone = url.to_string();
        let flight_key = format!("{}|{}|{}", url, start, end);
        // let flight_key = format!("{}|{}|{}", url, start, end); // 这行不需要了可以删掉或注释
        
        tokio::select! {
            result = self.route_fetch_images(&url_clone, start, end) => {
                match result {
                    Ok((meta, images)) if !images.is_empty() => {
                        let mut media_group = Vec::new();
                        let display_title = escape(&format!("{} (Pages {}-{})", meta.name, start, end));
                        let caption = link(&meta.link, &display_title);

                        for (i, (_img_meta, data)) in images.into_iter().enumerate() {
                            // 加上 .as_ref().to_owned() 嚴格適配 InputFile::memory 的 trait bound
                            let mut photo = InputMediaPhoto::new(InputFile::memory(data.as_ref().to_owned()));
                            if i == 0 {
                                photo = photo.caption(caption.clone()).parse_mode(ParseMode::MarkdownV2);
                            }
                            media_group.push(InputMedia::Photo(photo));
                        }

                        // 精準回覆用戶最初的指令
                        if let Ok(_) = bot.send_media_group(chat_id, media_group).reply_to_message_id(reply_msg_id).await {
                            // 靜默刪除 Syncing url... 提示
                            let _ = bot.delete_message(chat_id, prompt_msg_id).await;
                        } else {
                            let _ = bot.edit_message_text(chat_id, prompt_msg_id, escape("Failed to send media group.")).await;
                        }
                    },
                    Ok(_) => { let _ = bot.edit_message_text(chat_id, prompt_msg_id, escape("Failed: No images found in this range.")).await; },
                    Err(e) => { let _ = bot.edit_message_text(chat_id, prompt_msg_id, escape(&format!("Fetch failed: {}", e))).await; }
                }
            },
            _ = &mut cancel_rx => {
                 let _ = bot.edit_message_text(chat_id, prompt_msg_id, escape("Operation cancelled by user.")).await;
            }
        }
    }
}
