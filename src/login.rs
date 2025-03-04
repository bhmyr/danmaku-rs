//https://github.com/SocialSisterYi/bilibili-API-collect/blob/master/docs/login/cookie_refresh.md
use anyhow::{Context, Result, anyhow, bail};
use chrono::{Duration, Utc};
use log::{info, warn};
use qrcode::{QrCode, render::unicode};
use rand::thread_rng;
use regex::Regex;
use reqwest::{Client, Method, Url};
use rsa::{Oaep, RsaPublicKey, pkcs8::DecodePublicKey};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;
use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, OnceLock, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{fs, time};

const PASSPORT_API_BASE: &str = "https://passport.bilibili.com";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36";
const PUBLIC_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDLgd2OAkcGVtoE3ThUREbio0Eg
Uc/prcajMKXvkCKFCWhJYJcLkcM2DKKcSeFpD/j6Boy538YXnR6VhcuUJOhH2x71
nzPjfdTcqMz7djHum0qSZA0AyCBDABUqCrfNgCiJ00Ra7GmRj+YCK1NJEuewlb40
JNrRuoEUXpabUzGB8QIDAQAB
-----END PUBLIC KEY-----";
const QR_POLL_INTERVAL_SECONDS: u64 = 3;
const QR_TIMEOUT_MINUTES: i64 = 3;

// 全局认证状态
pub static AUTH: RwLock<OnceLock<Arc<AuthData>>> = RwLock::new(OnceLock::new());

/// 初始化认证系统
pub async fn init(auth_file: &str) -> Result<()> {
    let auth = Arc::new(AuthData::new(auth_file).await?);
    let _ = AUTH.write().unwrap().set(auth.clone());
    Ok(())
}

/// 获取当前Cookie
pub fn get_cookie() -> String {
    AUTH.read()
        .unwrap()
        .get()
        .expect("Auth 未初始化")
        .get_cookie()
}

/// 获取用户UID
pub fn get_uid() -> u64 {
    AUTH.read().unwrap().get().expect("Auth 未初始化").get_uid()
}

/// 主动刷新Cookie
pub async fn refresh_cookie() -> Result<()> {
    let mut auth = AUTH.write().unwrap();
    let auth_data = auth.get_mut().expect("Auth 未初始化");
    Arc::get_mut(auth_data)
        .ok_or_else(|| anyhow!("AuthData 被多个引用无法修改"))?
        .refresh_cookie()
        .await
}

/// API响应结构体
#[derive(Debug, Deserialize)]
struct APIResponse<T> {
    code: i32,
    message: String,
    #[serde(default)]
    data: T,
}

/// 认证数据结构体
#[derive(Debug, Serialize, Deserialize)]
pub struct AuthData {
    uid: u64,
    uname: String,
    refresh_token: String,
    cookies: String,
    #[serde(skip)]
    auth_file: String,
}

impl AuthData {
    /// 创建或加载认证数据
    async fn new(auth_file: &str) -> Result<Self> {
        let mut auth = Self {
            uid: 0,
            uname: String::new(),
            refresh_token: String::new(),
            cookies: String::new(),
            auth_file: auth_file.to_string(),
        };

        match auth.load().await {
            Ok(_) => info!("从文件加载认证信息成功"),
            Err(e) => {
                warn!("加载认证文件失败: {}. 需要重新登录", e);
                auth.qr_login().await.context("二维码登录失败")?;
            }
        }

        Ok(auth)
    }
    /// 获取Cookies
    pub fn get_cookie(&self) -> String {
        self.cookies.clone()
    }

    /// 获取用户ID
    pub fn get_uid(&self) -> u64 {
        self.uid
    }
    /// 保存认证数据到文件
    async fn save(&self) -> Result<()> {
        let data = toml::to_string_pretty(self).context("序列化认证数据失败")?;
        fs::write(&self.auth_file, data)
            .await
            .context("写入认证文件失败")?;
        Ok(())
    }

    /// 从文件加载认证数据
    async fn load(&mut self) -> Result<()> {
        let config_str = fs::read_to_string(&self.auth_file)
            .await
            .context("读取认证文件失败")?;

        let loaded: Self = toml::from_str(&config_str).context("解析TOML数据失败")?;
        let (uid, uname) = validate_cookies(&loaded.cookies)
            .await
            .context("Cookie验证失败")?;

        self.uid = uid;
        self.uname = uname;
        self.refresh_token = loaded.refresh_token;
        self.cookies = loaded.cookies;

        Ok(())
    }

    /// QR码登录流程
    async fn qr_login(&mut self) -> Result<()> {
        #[derive(Debug, Deserialize, Default)]
        struct QrCodeResponse {
            url: String,
            qrcode_key: String,
        }

        let api_url = format!("{}/x/passport-login/web/qrcode/generate", PASSPORT_API_BASE);
        let (resp, _) = send_request::<QrCodeResponse>(Method::GET, &api_url, None, None, None)
            .await
            .context("获取二维码失败")?;

        self.display_qr_code(&resp.url)?;
        self.poll_login_status(&resp.qrcode_key).await
    }

    /// 显示二维码到控制台
    fn display_qr_code(&self, url: &str) -> Result<()> {
        let code = QrCode::new(url.as_bytes()).context("生成QR码失败")?;
        let image = code
            .render::<unicode::Dense1x2>()
            .dark_color(unicode::Dense1x2::Dark)
            .light_color(unicode::Dense1x2::Light)
            .build();

        println!("请使用哔哩哔哩APP扫描二维码:");
        println!("{}", image);
        println!("备用访问地址: {}", url);
        Ok(())
    }

    /// 轮询登录状态
    async fn poll_login_status(&mut self, qr_key: &str) -> Result<()> {
        #[derive(Debug, Deserialize, Default)]
        struct QrPollResponse {
            code: i32,
            message: Option<String>,
            refresh_token: Option<String>,
        }
        let api_url = format!(
            "{}/x/passport-login/web/qrcode/poll?qrcode_key={}",
            PASSPORT_API_BASE, qr_key
        );

        let start_time = Utc::now();
        let timeout = Duration::minutes(QR_TIMEOUT_MINUTES);

        loop {
            if Utc::now() - start_time > timeout {
                bail!("二维码登录超时，请重新获取二维码");
            }

            let (resp, cookies) =
                send_request::<QrPollResponse>(Method::GET, &api_url, None, None, None)
                    .await
                    .context("轮询登录状态失败")?;

            match resp.code {
                0 => {
                    if let Some(refresh_token) = resp.refresh_token {
                        return self
                            .handle_login_success(cookies.unwrap_or_default(), &refresh_token)
                            .await;
                    } else {
                        bail!("登录成功但未收到refresh_token")
                    }
                }
                86038 => bail!("二维码已过期"),
                86090 => info!("已扫描，请在APP确认"),
                86101 => info!("等待扫描..."),
                code => bail!(
                    "未知状态码: {} ({})",
                    code,
                    resp.message.as_ref().unwrap_or(&"未知错误".to_string())
                ),
            }

            time::sleep(time::Duration::from_secs(QR_POLL_INTERVAL_SECONDS)).await;
        }
    }

    /// 处理登录成功逻辑
    async fn handle_login_success(&mut self, cookies: String, refresh_token: &str) -> Result<()> {
        let (uid, uname) = validate_cookies(&cookies).await.context("Cookie验证失败")?;
        self.uid = uid;
        self.uname = uname;
        self.cookies = cookies;
        self.refresh_token = refresh_token.to_string();
        self.save().await.context("保存登录信息失败")?;
        Ok(())
    }

    /// 刷新Cookies
    pub async fn refresh_cookie(&mut self) -> Result<()> {
        // 1. 旧cookie检查是否需要刷新
        if !self.need_refresh().await? {
            info!("当前Cookie有效，无需刷新");
            return Ok(());
        }

        // 2. 旧cookie 获取 旧csrf
        let csrf = extract_csrf(&self.cookies).context("获取CSRF失败")?;

        // 3. 生成correspond_path
        let correspond_path = generate_correspond_path()
            .await
            .context("生成correspond_path失败")?;

        // 4. 旧csrf / correspond_path 获取refresh_csrf
        let refresh_csrf = self
            .get_refresh_csrf(&correspond_path)
            .await
            .context("获取refresh_csrf失败")?;

        // 5. refresh_csrf / 旧csrf / 旧cookie / 旧refresh_token 刷新获取 新cookie和新refresh_token
        let (new_refresh_token, new_cookies) = self
            .perform_refresh(&csrf, &refresh_csrf)
            .await
            .context("刷新Cookie失败")?;

        // 6. 新csrf / 新cookie / 旧refresh_token 确认刷新
        self.confirm_refresh(&new_cookies)
            .await
            .context("确认刷新失败")?;

        // 7. 更新并保存认证数据
        self.refresh_token = new_refresh_token;
        self.cookies = new_cookies;
        self.save().await.context("保存刷新后的认证数据失败")?;

        info!("Cookie刷新成功");
        Ok(())
    }
    /// 检查是否需要刷新Cookie
    async fn need_refresh(&self) -> Result<bool> {
        let url = format!("{}/x/passport-login/web/cookie/info", PASSPORT_API_BASE);
        let headers = Some(HashMap::from([("Cookie", self.cookies.as_str())]));

        #[derive(Debug, Deserialize, Default)]
        struct RefreshInfo {
            refresh: bool,
        }

        let (resp, _) = send_request::<RefreshInfo>(Method::GET, &url, headers, None, None)
            .await
            .context("检查刷新状态失败")?;

        Ok(resp.refresh)
    }
    /// 获取refresh_csrf
    async fn get_refresh_csrf(&self, correspond_path: &str) -> Result<String> {
        let url = format!("https://www.bilibili.com/correspond/1/{}", correspond_path);
        let client = Client::builder()
            .cookie_store(true)
            .build()
            .context("创建HTTP客户端失败")?;

        let resp = client
            .get(&url)
            .header("Cookie", &self.cookies)
            .send()
            .await
            .context("请求correspond路径失败")?;

        let body = resp.text().await.context("读取响应内容失败")?;

        let re = Regex::new(r#"id="1-name">(.*?)</div>"#).context("编译正则表达式失败")?;
        re.captures(&body)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| anyhow!("未找到refresh_csrf"))
    }
    /// 执行刷新操作
    async fn perform_refresh(&self, csrf: &str, refresh_csrf: &str) -> Result<(String, String)> {
        let url = format!("{}/x/passport-login/web/cookie/refresh", PASSPORT_API_BASE);
        let headers = Some(HashMap::from([("Cookie", self.cookies.as_str())]));

        let body = vec![
            ("csrf", csrf),
            ("refresh_csrf", refresh_csrf),
            ("source", "main_web"),
            ("refresh_token", &self.refresh_token),
        ];

        #[derive(Debug, Deserialize, Default)]
        struct RefreshResponse {
            refresh_token: String,
        }

        let (resp, cookies) = send_request::<RefreshResponse>(
            Method::POST,
            &url,
            headers,
            None,
            Some(body.into_iter().collect()),
        )
        .await
        .context("刷新请求失败")?;

        Ok((resp.refresh_token, cookies.unwrap_or_default()))
    }

    /// 确认刷新
    async fn confirm_refresh(&self, cookies: &str) -> Result<()> {
        let csrf = extract_csrf(cookies).context("获取新CSRF失败")?;
        let url = format!("{}/x/passport-login/web/confirm/refresh", PASSPORT_API_BASE);
        let headers = Some(HashMap::from([("Cookie", cookies)]));

        let body = vec![("csrf", csrf), ("refresh_token", &self.refresh_token)];

        send_request::<()>(
            Method::POST,
            &url,
            headers,
            None,
            Some(body.into_iter().collect()),
        )
        .await
        .context("确认刷新请求失败")?;

        Ok(())
    }
}

async fn send_request<T>(
    method: Method,
    url: &str,
    headers: Option<HashMap<&str, &str>>,
    query: Option<HashMap<&str, &str>>,
    form_data: Option<HashMap<&str, &str>>,
) -> Result<(T, Option<String>)>
where
    T: DeserializeOwned + Default + Debug,
{
    let client = Client::new();
    let url = Url::parse(url).context("无效的URL")?;

    let mut request = client.request(method, url).header("User-Agent", USER_AGENT);

    // 添加头部
    if let Some(headers) = headers {
        for (k, v) in headers {
            request = request.header(k, v);
        }
    }

    // 添加查询参数
    if let Some(query) = query {
        request = request.query(&query);
    }

    // 处理表单数据
    if let Some(form) = form_data {
        request = request.form(&form);
    }

    let response = request.send().await.context("请求发送失败")?;

    // 处理Cookie
    let cookies = response
        .headers()
        .get_all("Set-Cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join("; ");

    // 处理响应状态
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("HTTP错误 {}: {}", status, body);
    }

    // 解析JSON
    let api_response: APIResponse<T> = response.json().await.context("响应解析失败")?;
    if api_response.code != 0 {
        bail!("API错误: {} ({})", api_response.code, api_response.message);
    }

    Ok((api_response.data, Some(cookies)))
}

/// 获取加密后的correspond_path
async fn generate_correspond_path() -> Result<String> {
    let public_key =
        RsaPublicKey::from_public_key_pem(PUBLIC_KEY_PEM).context("解析B站公钥失败")?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("获取系统时间失败")?
        .as_millis();

    let plaintext = format!("refresh_{}", timestamp);
    let mut rng = thread_rng();
    let padding = Oaep::new::<Sha256>();

    let encrypted = public_key
        .encrypt(&mut rng, padding, plaintext.as_bytes())
        .context("加密失败")?;

    Ok(hex::encode(encrypted))
}

/// 从Cookie中提取CSRF token
fn extract_csrf(cookies: &str) -> Result<&str> {
    cookies
        .split(';')
        .find_map(|c| {
            let mut parts = c.trim().splitn(2, '=');
            match (parts.next(), parts.next()) {
                (Some("bili_jct"), Some(value)) => Some(value),
                _ => None,
            }
        })
        .ok_or_else(|| anyhow!("未找到CSRF token"))
}

/// 验证Cookies是否有效
async fn validate_cookies(cookies: &str) -> Result<(u64, String)> {
    #[derive(Debug, Deserialize, Default)]
    struct UserInfo {
        #[serde(rename = "isLogin")]
        is_login: bool,
        mid: u64,
        uname: String,
    }
    let url = "https://api.bilibili.com/x/web-interface/nav";
    let headers = Some(HashMap::from([("Cookie", cookies)]));
    let (user_info, _) = send_request::<UserInfo>(Method::GET, url, headers, None, None)
        .await
        .context("验证Cookies失败")?;

    if user_info.is_login {
        info!(
            "用户已登录，MID: {}, 昵称: {}",
            user_info.mid, user_info.uname
        );
        Ok((user_info.mid, user_info.uname))
    } else {
        bail!("用户未登录")
    }
}
