use std::{
    fs::File,
    io::BufReader,
    path::Path,
    sync::{
        atomic::{AtomicU32, Ordering},
        OnceLock,
    },
    time::Duration,
};

use crate::BootArgs;
use actix_web::{
    error,
    middleware::Logger,
    web::{self, Json},
    App, Error, HttpResponse, HttpServer, Responder,
};
use actix_web_httpauth::extractors::bearer::BearerAuth;
use anyhow::Result;
use rand::Rng;
use reqwest::{
    header::{self, HeaderValue},
    StatusCode,
};
use rustls::ServerConfig;
use serde_json::{json, Value};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const KEEP_ALIVE: u8 = 75;
const CONNECTION_TIMEOUT: u8 = 10;
const TIMEOUT: u16 = 360;

// This struct represents state
struct AppState {
    api_key: Option<String>,
    dl_session: String,
}

pub struct Serve(pub BootArgs);

impl Serve {
    #[actix_web::main]
    pub async fn run(&self) -> Result<()> {
        // Init tracing
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "RUST_LOG=info".into()),
            )
            .with(tracing_subscriber::fmt::layer())
            .init();

        // Init client pool
        let client = Client::new(self.0.proxies.clone())?;
        let _ = CLIENT.set(client);

        let api_key = self.0.api_key.clone();
        let dl_session = self.0.dl_session.clone();

        api_key.as_ref().map(|_| {
            tracing::info!("API key is required");
        });

        tracing::info!("Starting server at {}", self.0.bind);

        // Start server
        let builder = HttpServer::new(move || {
            App::new()
                .wrap(
                    actix_cors::Cors::default()
                        .supports_credentials()
                        .allow_any_origin()
                        .allow_any_header()
                        .allow_any_method()
                        .max_age(3600),
                )
                .wrap(Logger::default())
                .app_data(web::Data::new(AppState {
                    api_key: api_key.clone(),
                    dl_session: dl_session.clone(),
                }))
                .route("/", web::get().to(manual_hello))
                .route("/translate", web::post().to(translate))
        })
        .client_request_timeout(Duration::from_secs(TIMEOUT as u64))
        .client_disconnect_timeout(Duration::from_secs(CONNECTION_TIMEOUT as u64))
        .keep_alive(Duration::from_secs(KEEP_ALIVE as u64));

        match (&self.0.tls_cert, &self.0.tls_key) {
            (Some(cert), Some(key)) => {
                let tls_config = Self::load_rustls_config(cert, key).await?;

                builder
                    .bind_rustls_0_22(self.0.bind, tls_config)?
                    .run()
                    .await?
            }
            _ => builder.bind(self.0.bind)?.run().await?,
        }

        Ok(())
    }

    async fn load_rustls_config<P: AsRef<Path>>(tls_cert: P, tls_key: P) -> Result<ServerConfig> {
        use rustls_pemfile::{certs, private_key};

        // load TLS key/cert files
        let cert_file = &mut BufReader::new(File::open(tls_cert)?);
        let key_file = &mut BufReader::new(File::open(tls_key)?);

        // load TLS certs and key
        // to create a self-signed temporary cert for testing:
        // `openssl req -x509 -newkey rsa:4096 -nodes -keyout key.pem -out cert.pem -days 365 -subj '/CN=localhost'`
        let tls_certs = certs(cert_file).collect::<Result<Vec<_>, _>>().unwrap();

        let keys = private_key(key_file)?
            .ok_or_else(|| anyhow::anyhow!("Could not locate EC/PKCS8/RSA private keys."))?;

        // set up TLS config options
        let tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(tls_certs, keys)
            .map_err(|e| anyhow::anyhow!("Could not set up TLS config: {}", e))?;

        Ok(tls_config)
    }
}

async fn manual_hello() -> impl Responder {
    HttpResponse::Ok().body("DeepL Free API, Developed by gngpp. Go to /translate with POST. http://github.com/gngpp/deeplx")
}

async fn translate(
    req: Json<PayloadFree>,
    auth: Option<BearerAuth>,
    state: web::Data<AppState>,
) -> actix_web::Result<impl Responder> {
    // Verify the API key
    verify_api_key(auth, &state).await?;
    let id = get_random_number() + 1;
    let number_alternative = 0.clamp(0, 3);

    let post_data = json!({
        "jsonrpc": "2.0",
        "method": "LMT_handle_texts",
        "id": id,
        "params": {
            "texts": [{
                "text": req.text,
                "requestAlternatives": number_alternative
            }],
            "splitting": "newlines",
            "lang": {
                "source_lang_user_selected": req.source_lang.to_uppercase(),
                "target_lang": req.target_lang.to_uppercase(),
            },
            "timestamp": get_timestamp(get_i_count(&req.text))?,
            "commonJobParams": {
                "wasSpoken": false,
                "transcribe_as": ""
            }
        },
    });

    let mut body = serde_json::to_string(&post_data)?;

    if (id + 5) % 29 == 0 || (id + 3) % 13 == 0 {
        body = body.replace("\"method\":\"", "\"method\" : \"");
    } else {
        body = body.replace("\"method\":\"", "\"method\": \"");
    }

    let resp = get_client()?
        .post("https://api.deepl.com/jsonrpc")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("dl_session={};", state.dl_session))
        .body(body)
        .send()
        .await
        .map_err(error::ErrorBadGateway)?;

    if resp.status() == StatusCode::TOO_MANY_REQUESTS {
        return Err(error::ErrorTooManyRequests(
                "Too many requests, your IP has been blocked by DeepL temporarily, please don't request it frequently in a short time."
            ));
    }

    let body = resp
        .error_for_status()
        .map_err(error::ErrorInternalServerError)?
        .json::<Value>()
        .await
        .map_err(error::ErrorBadGateway)?;

    let mut alternatives = Vec::new();

    let texts_zero = body
        .get("result")
        .map(|v| v.get("texts").map(|arr| arr.as_array()).flatten())
        .map(|v| v.map(|v| v.get(0)).flatten())
        .flatten();

    texts_zero.map(|v| v.as_array()).flatten().map(|arr| {
        for value in arr {
            value
                .get("alternatives")
                .map(|v| v.as_array())
                .flatten()
                .map(|arr| {
                    for value in arr {
                        value
                            .get("text")
                            .map(|v| v.as_str())
                            .flatten()
                            .map(|s| alternatives.push(s));
                    }
                });
        }
    });

    let data = texts_zero
        .map(|v| v.get("text"))
        .flatten()
        .map(|v| v.as_str())
        .flatten()
        .unwrap_or_default();

    let response = json!({
        "code": StatusCode::OK.as_u16(),
        "id": id,
        "data": data,
        "alternatives": alternatives,
        "source_lang": req.source_lang,
        "target_lang": req.target_lang,
        "method": "Free",
    });

    // json rpc translate
    Ok(HttpResponse::Ok().json(response))
}

/// Verify the API key
async fn verify_api_key(
    auth: Option<BearerAuth>,
    state: &web::Data<AppState>,
) -> Result<(), Error> {
    // Check if the API key is valid
    if let (Some(auth), Some(ref api_key)) = (auth, &state.api_key) {
        if auth.token().ne(api_key) {
            return Err(actix_web::error::ErrorUnauthorized(
                "You are not authorized",
            ));
        }
    }

    Ok(())
}

use std::time::{SystemTime, UNIX_EPOCH};

/// Get i count
pub fn get_i_count(translate_text: &str) -> usize {
    translate_text.matches('i').count()
}

/// Get random number
pub fn get_random_number() -> u64 {
    let mut rng = rand::thread_rng();
    let num: u64 = rng.gen_range(0..99999) + 8300000;
    num * 1000
}

/// Get timestamp
pub fn get_timestamp(i_count: usize) -> actix_web::Result<u128> {
    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .map_err(error::ErrorInternalServerError)?;
    let in_ms = since_the_epoch.as_millis();

    if i_count == 0 {
        Ok(in_ms)
    } else {
        let i_count = i_count as u128;
        Ok(in_ms - (in_ms % i_count) + i_count)
    }
}

static CLIENT: OnceLock<Client> = OnceLock::new();

fn get_client() -> actix_web::Result<reqwest::Client> {
    Ok(CLIENT
        .get()
        .ok_or_else(|| error::ErrorInternalServerError("Failed to get the client"))?
        .next())
}

struct Client(AtomicU32, Vec<reqwest::Client>);

impl Client {
    fn new(proxies: Option<Vec<String>>) -> Result<Self> {
        let mut clients = Vec::new();

        if let Some(proxies) = proxies {
            for proxy in proxies {
                let client = Self::build_client(Some(proxy))?;
                clients.push(client);
            }
        } else {
            let client = Self::build_client(None)?;
            clients.push(client);
        }
        Ok(Self(AtomicU32::new(0), clients))
    }

    fn build_client(proxy: Option<String>) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder()
            .default_headers((|| {
                let mut headers = header::HeaderMap::new();
                headers.insert(header::USER_AGENT, HeaderValue::from_static("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36"));
                headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
                headers.insert(
                    header::ACCEPT_LANGUAGE,
                    HeaderValue::from_static("en-US,en;q=0.9"),
                );
                headers.insert(
                    header::ACCEPT_ENCODING,
                    HeaderValue::from_static("gzip, deflate, br"),
                );
                headers.insert(
                    header::ORIGIN,
                    HeaderValue::from_static("https://www.deepl.com"),
                );
                headers.insert(
                    header::REFERER,
                    HeaderValue::from_static("https://www.deepl.com/"),
                );
                headers
            })())
            .timeout(Duration::from_secs(TIMEOUT as u64))
            .connect_timeout(Duration::from_secs(CONNECTION_TIMEOUT as u64))
            .tcp_keepalive(Duration::from_secs(KEEP_ALIVE as u64))
            .redirect(reqwest::redirect::Policy::none());

        if let Some(proxy) = proxy {
            builder = builder.proxy(reqwest::Proxy::all(&proxy)?);
        }

        builder.build().map_err(Into::into)
    }
    // Round-robin client
    fn next(&self) -> reqwest::Client {
        let pool = &self.1;
        if self.1.len() == 1 {
            self.1[0].clone()
        } else {
            let len = self.1.len() as u32;
            let mut old = self.0.load(Ordering::Relaxed);
            let mut new;
            loop {
                new = (old + 1) % len;
                match self
                    .0
                    .compare_exchange_weak(old, new, Ordering::SeqCst, Ordering::Relaxed)
                {
                    Ok(_) => break,
                    Err(x) => old = x,
                }
            }
            pool[new as usize].clone()
        }
    }
}

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct PayloadFree {
    pub text: String,
    #[serde(default = "default_source_text")]
    pub source_lang: String,
    #[serde(default = "default_target_lang")]
    pub target_lang: String,
}

fn default_source_text() -> String {
    String::from("AUTO")
}

fn default_target_lang() -> String {
    String::from("ZH")
}