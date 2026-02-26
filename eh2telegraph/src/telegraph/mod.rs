/// Telegraph API Client
pub use error::TelegraphError;
#[macro_use]
pub mod types;
pub const MAX_SINGLE_FILE_SIZE: usize = 5 * 1024 * 1024;

mod error;

use std::{borrow::Cow, sync::Arc};

use reqwest::{
    multipart::{Form, Part},
    Client, Response,
};
use serde::Serialize;

use crate::http_client::HttpRequestBuilder;

use self::{
    error::{ApiResult, UploadResult},
    types::{MediaInfo, Node, Page, PageCreate, PageEdit},
};

const TITLE_LENGTH_MAX: usize = 200;

#[derive(Debug, Clone)]
pub struct Telegraph<T, C = Client> {
    client: C,
    access_token: T,
}

pub trait AccessToken {
    fn token(&self) -> &str;
    fn select_token(&self, _path: &str) -> &str {
        Self::token(self)
    }
}

#[derive(Debug, Clone)]
pub struct SingleAccessToken(pub Arc<String>);

#[derive(Debug, Clone)]
pub struct RandomAccessToken(pub Arc<Vec<String>>);

impl AccessToken for SingleAccessToken {
    fn token(&self) -> &str {
        &self.0
    }
}

impl From<String> for SingleAccessToken {
    fn from(s: String) -> Self {
        Self(Arc::new(s))
    }
}

impl AccessToken for RandomAccessToken {
    fn token(&self) -> &str {
        use rand::prelude::SliceRandom;
        self.0
            .choose(&mut rand::thread_rng())
            .expect("token list must contains at least one element")
    }
}

impl From<String> for RandomAccessToken {
    fn from(s: String) -> Self {
        Self(Arc::new(vec![s]))
    }
}

impl From<Vec<String>> for RandomAccessToken {
    fn from(ts: Vec<String>) -> Self {
        assert!(!ts.is_empty());
        Self(Arc::new(ts))
    }
}

macro_rules! execute {
    ($send: expr) => {
        $send
            .send()
            .await
            .and_then(Response::error_for_status)?
            .json::<ApiResult<_>>()
            .await?
            .into()
    };
}

#[derive(Debug, Clone, PartialEq, Eq, derive_more::From, derive_more::Into)]
pub struct TelegraphToken(Arc<String>);

impl<T> Telegraph<T, Client> {
    pub fn new<AT>(access_token: AT) -> Telegraph<T, Client>
    where
        AT: Into<T>,
    {
        Telegraph {
            client: Client::new(),
            access_token: access_token.into(),
        }
    }
}

impl<T, C> Telegraph<T, C> {
    pub fn with_proxy<P: HttpRequestBuilder + 'static>(self, proxy: P) -> Telegraph<T, P> {
        Telegraph {
            client: proxy,
            access_token: self.access_token,
        }
    }
}

impl<T, C> Telegraph<T, C>
where
    T: AccessToken,
    C: HttpRequestBuilder,
{
    pub async fn create_page(&self, page: &PageCreate) -> Result<Page, TelegraphError> {
        #[derive(Serialize)]
        struct PageCreateShadow<'a> {
            pub title: &'a str,
            pub content: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            pub author_name: &'a Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            pub author_url: &'a Option<String>,
        }

        #[derive(Serialize)]
        struct PagePostWithToken<'a> {
            access_token: &'a str,
            #[serde(flatten)]
            page: &'a PageCreateShadow<'a>,
        }

        let title = page.title.chars().take(TITLE_LENGTH_MAX).collect::<String>();
        let content = serde_json::to_string(&page.content).expect("unable to content serialize json");
        
        let to_post = PagePostWithToken {
            access_token: self.access_token.token(),
            page: &PageCreateShadow {
                title: &title,
                content: &content,
                author_name: &page.author_name,
                author_url: &page.author_url,
            },
        };
        execute!(self.client.post_builder("https://api.telegra.ph/createPage").form(&to_post))
    }

    pub async fn edit_page(&self, page: &PageEdit) -> Result<Page, TelegraphError> {
        #[derive(Serialize)]
        struct PageEditShadow<'a> {
            pub title: &'a str,
            pub path: &'a str,
            pub content: &'a str, // <--- 修复处：将 &'a Vec<Node> 修正为 &'a str
            #[serde(skip_serializing_if = "Option::is_none")]
            pub author_name: &'a Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            pub author_url: &'a Option<String>,
        }

        #[derive(Serialize)]
        struct PageEditWithToken<'a> {
            access_token: &'a str,
            #[serde(flatten)]
            page: &'a PageEditShadow<'a>,
        }

        let title = page.title.chars().take(TITLE_LENGTH_MAX).collect::<String>();
        // <--- 修复处：主动将节点序列化为 JSON 字符串
        let content_str = serde_json::to_string(&page.content).expect("unable to content serialize json");
        
        let to_post = PageEditWithToken {
            access_token: self.access_token.select_token(&page.path),
            page: &PageEditShadow {
                title: &title,
                path: &page.path,
                content: &content_str, 
                author_name: &page.author_name,
                author_url: &page.author_url,
            },
        };
        execute!(self.client.post_builder("https://api.telegra.ph/editPage").form(&to_post))
    }

    pub async fn get_page(&self, path: &str) -> Result<Page, TelegraphError> {
        #[derive(Serialize)]
        struct PageGet<'a> {
            path: &'a str,
            #[serde(flatten)]
            return_content: Option<bool>,
        }

        let to_post = PageGet {
            path,
            return_content: Some(true),
        };
        execute!(self.client.post_builder("https://api.telegra.ph/getPage").form(&to_post))
    }

    pub async fn upload<IT, I>(&self, files: IT) -> Result<Vec<MediaInfo>, TelegraphError>
    where
        IT: IntoIterator<Item = I>,
        I: Into<Cow<'static, [u8]>>,
    {
        let mut results = Vec::new();

        for data in files.into_iter() {
            let form = Form::new()
                .text("reqtype", "fileupload")
                .text("userhash", "")
                .part("fileToUpload", Part::bytes(data).file_name("image.jpg"));

            let response = self
                .client
                .post_builder("https://catbox.moe/user/api.php")
                .multipart(form)
                .send()
                .await
                .and_then(Response::error_for_status)?;

            let url = response.text().await?;

            if url.starts_with("https://files.catbox.moe/") {
                results.push(MediaInfo { src: url });
            } else {
                return Err(TelegraphError::Server);
            }
        }

        Ok(results)
    }
}
