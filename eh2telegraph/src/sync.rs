use crate::{
    buffer::{DataSized, ImageBuffer},
    collector::{
        AlbumMeta, Collector, ImageData, ImageMeta, Param, Registry, URL_FROM_TEXT_RE,
        URL_FROM_URL_RE,
    },
    http_proxy::ProxiedClient,
    storage::{cloudflare_kv::CFStorage, KVStorage},
    stream::{AsyncStream, Buffered},
    telegraph::{
        types::{Node, NodeElement, NodeElementAttr, Page, PageCreate, PageEdit, Tag},
        RandomAccessToken, Telegraph, TelegraphError, MAX_SINGLE_FILE_SIZE,
    },
    util::match_first_group,
};

const ERR_THRESHOLD: usize = 10;
const BATCH_LEN_THRESHOLD: usize = 20;
const BATCH_SIZE_THRESHOLD: usize = 5 * 1024 * 1024;
const DEFAULT_CONCURRENT: usize = 20;

#[derive(thiserror::Error, Debug)]
pub enum UploadError<SE> {
    #[error("stream error {0}")]
    Stream(SE),
    #[error("telegraph error {0}")]
    Reqwest(#[from] TelegraphError),
    #[error("No images found in the specified range")]
    EmptyResult,
}

pub struct Synchronizer<C = CFStorage> {
    tg: Telegraph<RandomAccessToken, ProxiedClient>,
    limit: Option<usize>,

    author_name: Option<String>,
    author_url: Option<String>,
    cache_ttl: Option<usize>,

    registry: Registry,
    cache: C,
}

impl<CACHE> Synchronizer<CACHE>
where
    CACHE: KVStorage<String>,
{
    const DEFAULT_CACHE_TTL: usize = 3600 * 24 * 45;

    pub fn new(
        tg: Telegraph<RandomAccessToken, ProxiedClient>,
        registry: Registry,
        cache: CACHE,
    ) -> Self {
        Self {
            tg,
            limit: None,
            author_name: None,
            author_url: None,
            cache_ttl: None,
            registry,
            cache,
        }
    }

    pub fn with_concurrent_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn with_author<S: Into<String>>(mut self, name: Option<S>, url: Option<S>) -> Self {
        self.author_name = name.map(Into::into);
        self.author_url = url.map(Into::into);
        self
    }

    pub fn with_cache_ttl(mut self, ttl: Option<usize>) -> Self {
        self.cache_ttl = ttl;
        self
    }

    pub async fn delete_cache(&self, key: &str) -> anyhow::Result<()> {
        self.cache.delete(key).await
    }

    /// 專供小範圍圖片直出：只下載並返回圖片二進制數據，跳過所有 Telegraph 邏輯
    pub async fn fetch_images_only<C: Collector>(
        &self,
        path: String,
        start_page: usize,
        end_page: usize,
    ) -> anyhow::Result<(AlbumMeta, Vec<(ImageMeta, ImageData)>)>
    where
        Registry: Param<C>,
        C::FetchError: Into<anyhow::Error> + Send + 'static,
        C::StreamError:
            Into<anyhow::Error> + std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
        C::ImageStream: Send + 'static,
        <C::ImageStream as AsyncStream>::Future: Send + 'static,
    {
        let path = path.trim_end_matches('/').to_string();
        let collector: &C = self.registry.get();
        let (meta, mut stream) = collector.fetch(path).await.map_err(Into::into)?;

        // 高效跳過不需要的開頭頁（零開銷網絡）
        for _ in 1..start_page {
            if stream.next().is_none() {
                break;
            }
        }

        let mut images = Vec::new();
        let mut current_idx = start_page;

        // 收集指定範圍內的圖片
        while let Some(fut) = stream.next() {
            if current_idx > end_page {
                break;
            }
            if let Ok(data) = fut.await.map_err(Into::into) {
                images.push(data);
            }
            current_idx += 1;
        }

        Ok((meta, images))
    }

    pub async fn sync<C: Collector>(
        &self,
        path: String,
        start_page: Option<usize>,
        end_page: Option<usize>,
    ) -> anyhow::Result<String>
    where
        Registry: Param<C>,
        C::FetchError: Into<anyhow::Error> + Send + 'static,
        C::StreamError:
            Into<anyhow::Error> + std::fmt::Debug + std::fmt::Display + Send + Sync + 'static,
        C::ImageStream: Send + 'static,
        <C::ImageStream as AsyncStream>::Future: Send + 'static,
    {
        let path = path.trim_end_matches('/').to_string();
        
        // 緩存隔離：不同範圍的請求不能命中全量緩存
        let mut cache_key = format!("{}|{}", C::name(), path);
        if start_page.is_some() || end_page.is_some() {
            cache_key = format!("{}|s:{:?}|e:{:?}", cache_key, start_page, end_page);
        }
        let cache_key = cache_key.replace("exhentai", "e-hentai");

        if let Ok(Some(v)) = self.cache.get(&cache_key).await {
            tracing::info!("[cache] hit key {cache_key}");
            return Ok(v);
        }
        tracing::info!("[cache] miss key {cache_key}");

        let collector: &C = self.registry.get();
        let (meta, stream) = collector.fetch(path).await.map_err(Into::into)?;
        let page = self
            .sync_stream(meta, stream, start_page, end_page)
            .await
            .map_err(anyhow::Error::from)?;

        let _ = self
            .cache
            .set(
                cache_key,
                page.url.clone(),
                Some(self.cache_ttl.unwrap_or(Self::DEFAULT_CACHE_TTL)),
            )
            .await;
        Ok(page.url)
    }

    pub async fn sync_stream<S, SE>(
        &self,
        meta: AlbumMeta,
        mut stream: S,
        start_page: Option<usize>,
        end_page: Option<usize>,
    ) -> Result<Page, UploadError<SE>>
    where
        SE: Send + std::fmt::Debug + 'static,
        S: AsyncStream<Item = Result<(ImageMeta, ImageData), SE>>,
        S::Future: Send + 'static,
    {
        let start_idx = start_page.unwrap_or(1);
        for _ in 1..start_idx {
            if stream.next().is_none() {
                break;
            }
        }

        let buffered_stream = Buffered::new(stream, self.limit.unwrap_or(DEFAULT_CONCURRENT));
        let r = self.inner_sync_stream(meta, buffered_stream, start_idx, end_page).await;
        match &r {
            Ok(p) => tracing::info!("[sync] sync success with url {}", p.url),
            Err(e) => tracing::error!("[sync] sync fail! {e:?}"),
        }
        r
    }

    async fn inner_sync_stream<S, SE>(
        &self,
        meta: AlbumMeta,
        mut stream: S,
        start_idx: usize,
        end_idx: Option<usize>,
    ) -> Result<Page, UploadError<SE>>
    where
        S: AsyncStream<Item = Result<(ImageMeta, ImageData), SE>>,
    {
        let mut err_count = 0;
        let mut uploaded = Vec::new();
        let mut buffer = ImageBuffer::new();
        let mut current_idx = start_idx;
        let mut finished_fetching = false;

        loop {
            if !finished_fetching {
                while let Some(fut) = stream.next() {
                    // 提前截斷流
                    if let Some(e) = end_idx {
                        if current_idx > e {
                            finished_fetching = true;
                            break;
                        }
                    }

                    let data = match fut.await {
                        Err(e) => {
                            err_count += 1;
                            if err_count > ERR_THRESHOLD {
                                return Err(UploadError::Stream(e));
                            }
                            continue;
                        }
                        Ok(d) => {
                            err_count = 0;
                            d
                        }
                    };

                    if data.1.len() >= MAX_SINGLE_FILE_SIZE {
                        tracing::error!("Too big file, discarded. Meta: {:?}", data.0);
                        current_idx += 1;
                        continue;
                    }

                    buffer.push(data);
                    current_idx += 1;

                    if buffer.len() > BATCH_LEN_THRESHOLD || buffer.size() > BATCH_SIZE_THRESHOLD {
                        break;
                    }
                }
            }
            
            if buffer.is_empty() {
                break;
            }

            let (full_data, size) = buffer.swap();
            let image_count = full_data.len();
            tracing::debug!("download {image_count} images with size {size}, will upload them");

            let (meta, data) = full_data
                .into_iter()
                .map(|(a, b)| (a, b.as_ref().to_owned()))
                .unzip::<_, _, Vec<_>, Vec<_>>();
            let medium = self.tg.upload(data).await?;
            err_count = 0;

            uploaded.extend(
                meta.into_iter()
                    .zip(medium.into_iter().map(|x| x.src))
                    .map(|(meta, src)| UploadedImage { meta, src }),
            );
        }

        if uploaded.is_empty() {
            return Err(UploadError::EmptyResult);
        }

        const PAGE_SIZE_LIMIT: usize = 48 * 1024;
        let mut chunks = Vec::with_capacity(8);
        chunks.push(Vec::new());
        let mut last_chunk_size = 0;
        for item in uploaded.into_iter().map(Into::<Node>::into) {
            let item_size = item.estimate_size();
            if last_chunk_size + item_size > PAGE_SIZE_LIMIT {
                chunks.push(Vec::new());
                last_chunk_size = 0;
            }
            last_chunk_size += item_size;
            chunks.last_mut().unwrap().push(item);
        }

        let mut last_page: Option<Page> = None;
        let original_title = meta.name.replace('|', "");
        
        // 生成用於污染 URL 的偽造標題
        let mut title_for_creation = original_title.clone();
        if let (Some(s), Some(e)) = (Some(start_idx), end_idx) {
            if s > 1 || e > 1 {
                title_for_creation = format!("{} {}-{}", original_title, s, e);
            }
        }

        while let Some(last_chunk) = chunks.pop() {
            let mut content = last_chunk;
            write_footer(
                &mut content,
                meta.link.as_str(),
                last_page.as_ref().map(|p| p.url.as_str()),
            );
            
            let current_fake_title = match chunks.len() {
                0 => title_for_creation.clone(),
                n => format!("{}-Page{}", title_for_creation, n + 1),
            };
            let current_clean_title = match chunks.len() {
                0 => original_title.clone(),
                n => format!("{}-Page{}", original_title, n + 1),
            };

            tracing::debug!("create page with content: {content:?}");
            let need_edit = title_for_creation != original_title;

            // 第一步：創建含有數字 URL 的頁面
            let mut page = self
                .tg
                .create_page(&PageCreate {
                    title: current_fake_title,
                    content: content.clone(),
                    author_name: self
                        .author_name
                        .clone()
                        .or_else(|| meta.authors.as_ref().map(|x| x.join(", "))),
                    author_url: self.author_url.clone(),
                })
                .await
                .map_err(UploadError::Reqwest)?;

            // 第二步：修改回原本乾淨的標題
            if need_edit {
                page = self
                    .tg
                    .edit_page(&PageEdit {
                        title: current_clean_title,
                        path: page.path.clone(),
                        content,
                        author_name: self
                            .author_name
                            .clone()
                            .or_else(|| meta.authors.as_ref().map(|x| x.join(", "))),
                        author_url: self.author_url.clone(),
                    })
                    .await
                    .map_err(UploadError::Reqwest)?;
            }

            last_page = Some(page);
        }
        Ok(last_page.unwrap())
    }
}

fn write_footer(content: &mut Vec<Node>, original_link: &str, next_page: Option<&str>) {
    if let Some(page) = next_page {
        content.push(np!(na!(@page, nt!("Next Page"))));
    }
    content.push(np!(
        nt!("Generated by "),
        na!(@"https://github.com/qini7-sese/eh2telegraph", nt!("eh2telegraph"))
    ));
    content.push(np!(
        nt!("Original link: "),
        na!(@original_link, nt!(original_link))
    ));
}

impl Synchronizer {
    pub fn match_url_from_text(content: &str) -> Option<&str> {
        match_first_group(&URL_FROM_TEXT_RE, content)
    }

    pub fn match_url_from_url(content: &str) -> Option<&str> {
        match_first_group(&URL_FROM_URL_RE, content)
    }
}

impl DataSized for (ImageMeta, ImageData) {
    #[inline]
    fn size(&self) -> usize {
        self.1.size()
    }
}

struct UploadedImage {
    #[allow(unused)]
    meta: ImageMeta,
    src: String,
}

impl From<UploadedImage> for Node {
    fn from(i: UploadedImage) -> Self {
        Node::new_image(&i.src)
    }
}
