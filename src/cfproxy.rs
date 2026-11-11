use crate::config::*;
use crate::ws::{is_http_status_error, ws_connect_once_port, RawWebSocket, WsError};
use crate::{ldebug, lerror, linfo, lwarn};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use once_cell::sync::Lazy;
static CFPROXY_SEM: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(CFPROXY_GLOBAL_PARALLEL));

#[derive(Clone, Debug)]
pub struct CfFastRoute {
    pub clean_ip: Option<String>,
    pub port: u16,
    pub secure: bool,
    pub expires_at: Instant,
}

static CF_FAST_ROUTE: Lazy<parking_lot::RwLock<Option<CfFastRoute>>> =
    Lazy::new(|| parking_lot::RwLock::new(None));

static CF_BAD_ROUTES: Lazy<parking_lot::RwLock<std::collections::HashMap<(String, u16), Instant>>> =
    Lazy::new(|| parking_lot::RwLock::new(std::collections::HashMap::new()));

static PORT80_SHORT_DROPS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Штраф на весь порт 80 хранится ОТДЕЛЬНО от CF_BAD_ROUTES.
/// Раньше `CF_BAD_ROUTES.write().clear()` (в т.ч. из notify_network_changed
/// и из ветки "все кандидаты плохие") мгновенно отменял "приоритет TLS 443
/// на 5 мин", и ядро снова уходило на режущийся оператором :80.
static PORT80_PENALTY_UNTIL: Lazy<parking_lot::RwLock<Option<Instant>>> =
    Lazy::new(|| parking_lot::RwLock::new(None));

/// Анти-шторм: если за короткое окно свалилось много CF-попыток,
/// на несколько секунд прекращаем долбить все 20 доменов и даём уйти
/// в прямой TCP/SNI fallback вместо сотен `CF fail ...` в секунду.
static CF_FAILURE_WINDOW: Lazy<parking_lot::RwLock<(Instant, usize)>> =
    Lazy::new(|| parking_lot::RwLock::new((Instant::now(), 0)));
static CF_COOLDOWN_UNTIL: Lazy<parking_lot::RwLock<Option<Instant>>> =
    Lazy::new(|| parking_lot::RwLock::new(None));
static CF_LOG_THROTTLE: Lazy<parking_lot::RwLock<std::collections::HashMap<String, Instant>>> =
    Lazy::new(|| parking_lot::RwLock::new(std::collections::HashMap::new()));

const PORT80_PENALTY: Duration = Duration::from_secs(300);
const CF_STORM_THRESHOLD: usize = 12;
const CF_STORM_WINDOW: Duration = Duration::from_secs(3);
const CF_STORM_COOLDOWN: Duration = Duration::from_secs(8);
const CF_LOG_INTERVAL: Duration = Duration::from_secs(2);

pub fn port80_penalty_active() -> bool {
    match *PORT80_PENALTY_UNTIL.read() {
        Some(until) => Instant::now() < until,
        None => false,
    }
}

pub fn cf_network_cooldown_active() -> bool {
    match *CF_COOLDOWN_UNTIL.read() {
        Some(until) => Instant::now() < until,
        None => false,
    }
}

pub fn note_cf_attempt_success() {
    *CF_FAILURE_WINDOW.write() = (Instant::now(), 0);
    *CF_COOLDOWN_UNTIL.write() = None;
}

pub fn note_cf_attempt_failure() {
    let now = Instant::now();
    let mut window = CF_FAILURE_WINDOW.write();
    if now.saturating_duration_since(window.0) > CF_STORM_WINDOW {
        window.0 = now;
        window.1 = 0;
    }
    window.1 += 1;
    if window.1 >= CF_STORM_THRESHOLD {
        *CF_COOLDOWN_UNTIL.write() = Some(now + CF_STORM_COOLDOWN);
        lwarn!(
            " CF шторм отказов: пауза {:.0}с, уходим в прямой fallback",
            CF_STORM_COOLDOWN.as_secs_f64()
        );
    }
}

/// Не пишем сотни одинаковых `CF fail <domain>: timeout` подряд.
pub fn should_log_cf_error(msg: &str) -> bool {
    let now = Instant::now();
    let mut throttled = CF_LOG_THROTTLE.write();
    if let Some(&last) = throttled.get(msg) {
        if now.saturating_duration_since(last) < CF_LOG_INTERVAL {
            return false;
        }
    }
    throttled.insert(msg.to_string(), now);
    if throttled.len() > 256 {
        throttled.retain(|_, t| now.saturating_duration_since(*t) < Duration::from_secs(8));
    }
    true
}

/// Смена сети НЕ отменяет ни штраф порта 80, ни счётчик сбросов после 101.
pub fn clear_cf_fast_route() {
    *CF_FAST_ROUTE.write() = None;
    CF_BAD_ROUTES.write().clear();
}

fn is_cf_route_bad(dial_addr: &str, port: u16) -> bool {
    if port == 80 && port80_penalty_active() {
        return true;
    }
    let now = Instant::now();
    let guard = CF_BAD_ROUTES.read();
    if let Some(&until) = guard.get(&(dial_addr.to_string(), port)) {
        if now < until {
            return true;
        }
    }
    if let Some(&until) = guard.get(&("*".to_string(), port)) {
        if now < until {
            return true;
        }
    }
    false
}

/// Вызывается, если WS-соединение через CF оборвалось сразу после рукопожатия (< 6с и < 20КБ),
/// что характерно для сброса ТСПУ (16KB блок или DPI-сброс порта 80 после HTTP 101).
pub fn mark_cf_route_unstable(dial_addr: &str, port: u16) {
    if dial_addr.is_empty() {
        return;
    }
    {
        let mut fast = CF_FAST_ROUTE.write();
        if let Some(r) = fast.as_ref() {
            if r.port == port || r.clean_ip.as_deref() == Some(dial_addr) {
                *fast = None;
            }
        }
    }
    let now = Instant::now();
    let mut penalize_port80 = false;
    {
        let mut bad = CF_BAD_ROUTES.write();
        bad.insert((dial_addr.to_string(), port), now + Duration::from_secs(180));
        if port == 80 {
            let drops = PORT80_SHORT_DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if drops >= 2 {
                bad.insert(("*".to_string(), 80), now + PORT80_PENALTY);
                penalize_port80 = true;
            }
        }
    }
    if penalize_port80 {
        *PORT80_PENALTY_UNTIL.write() = Some(now + PORT80_PENALTY);
        lwarn!(" CF port 80 сбрасывается оператором после 101 -> приоритет TLS 443 на 5 мин");
    }
}

/// Фиксирует рабочий маршрут в CF_FAST_ROUTE только после того, как через него реально прошёл трафик
/// (больше порога 16KB ТСПУ или стабильная сессия).
pub fn confirm_cf_route_healthy(dial_addr: &str, port: u16, secure: bool) {
    if dial_addr.is_empty() {
        return;
    }
    if port == 80 {
        PORT80_SHORT_DROPS.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    let is_clean = CF_CLEAN_IPS.iter().any(|&ip| ip == dial_addr);
    *CF_FAST_ROUTE.write() = Some(CfFastRoute {
        clean_ip: if is_clean {
            Some(dial_addr.to_string())
        } else {
            None
        },
        port,
        secure,
        expires_at: Instant::now() + Duration::from_secs(600),
    });
}

// ---------------------------------------------------------------------------
// Domain decoding & normalization
// ---------------------------------------------------------------------------

pub fn decode_cf_domain(s: &str) -> String {
    if !s.ends_with(".com") {
        return s.to_string();
    }
    let suffix = ".co.uk";
    let p = &s[..s.len() - 4];
    // Кодированные встроенные домены всегда состоят из одной метки до ".com"
    if p.contains('.') || p.contains('-') {
        return s.to_string();
    }
    let mut n = 0i32;
    for c in p.chars() {
        if (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') {
            n += 1;
        }
    }
    let mut result: Vec<u8> = Vec::new();
    for &c in p.as_bytes() {
        if c >= b'a' && c <= b'z' {
            let v = (((c - b'a') as i32 - n % 26 + 26) % 26) as u8 + b'a';
            result.push(v);
        } else if c >= b'A' && c <= b'Z' {
            let v = (((c - b'A') as i32 - n % 26 + 26) % 26) as u8 + b'A';
            result.push(v);
        } else {
            result.push(c);
        }
    }
    let mut out = String::from_utf8_lossy(&result).to_string();
    out.push_str(suffix);
    out
}

/// Нормализует встроенный кодированный домен (из CFPROXY_ENC или GitHub-списка Flowseal).
pub fn normalize_encoded_cf_domain(s: &str) -> String {
    let mut decoded = decode_cf_domain(s.trim()).trim().to_lowercase();
    while decoded.ends_with('.') {
        decoded.pop();
    }
    if decoded.is_empty() || !decoded.ends_with(".co.uk") {
        return String::new();
    }
    decoded
}

/// Универсальная нормализация домена: поддерживает как встроенные кодированные домены (.co.uk),
/// так и ЛЮБЫЕ пользовательские домены (.ru, .com, .xyz, .workers.dev, .pages.dev и т.д.).
pub fn normalize_cf_domain(s: &str) -> String {
    let cleaned = clean_domain_input(s);
    if cleaned.is_empty() {
        return String::new();
    }
    // Если строка входит в список встроенных зашифрованных токенов CFPROXY_ENC — декодируем её.
    if CFPROXY_ENC.iter().any(|&enc| enc.eq_ignore_ascii_case(&cleaned)) {
        return normalize_encoded_cf_domain(&cleaned);
    }
    if is_valid_domain(&cleaned) {
        return cleaned;
    }
    String::new()
}

/// Определяет, является ли домен serverless/worker-хостом (не использующим поддомены kws1..5).
pub fn is_worker_style_domain(domain: &str) -> bool {
    let d = domain.trim().to_lowercase();
    d.ends_with(".workers.dev")
        || d.ends_with(".pages.dev")
        || d.ends_with(".yandexcloud.net")
        || d.ends_with(".vercel.app")
        || d.ends_with(".netlify.app")
}

pub fn default_cfproxy_domains() -> Vec<String> {
    let mut domains = Vec::with_capacity(CFPROXY_ENC.len());
    for enc in CFPROXY_ENC {
        let d = normalize_encoded_cf_domain(enc);
        if !d.is_empty() {
            domains.push(d);
        }
    }
    domains
}

pub fn merge_cfproxy_domains(lists: &[Vec<String>]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for list in lists {
        for raw in list {
            let d = normalize_cf_domain(raw);
            if d.is_empty() || seen.contains(&d) {
                continue;
            }
            // Дополнительно валидируем как в _normalize_domain_pool оригинала.
            if !crate::config::is_valid_domain(&d) {
                continue;
            }
            seen.insert(d.clone());
            merged.push(d);
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// 429 cooldown logic
// ---------------------------------------------------------------------------

pub fn clear_cfproxy_429_cooldowns() {
    CFPROXY_429.write().clear();
}

pub fn clear_cfproxy_429_cooldown(domain: &str) {
    let d = normalize_cf_domain(domain);
    if d.is_empty() {
        return;
    }
    CFPROXY_429.write().remove(&d);
}

pub fn retry_after_delay(err: &WsError) -> Duration {
    let h = match err.handshake() {
        Some(h) => h,
        None => return Duration::ZERO,
    };
    let retry_after = h.headers.get("retry-after").map(|s| s.trim()).unwrap_or("");
    if retry_after.is_empty() {
        return Duration::ZERO;
    }
    if let Ok(seconds) = retry_after.parse::<i64>() {
        if seconds > 0 {
            return Duration::from_secs(seconds as u64);
        }
    }
    // http date parse (best-effort): пропускаем, как маловероятный кейс
    Duration::ZERO
}

pub fn next_cfproxy_429_cooldown_delay(prev: &Cfproxy429State, retry_after: Duration) -> Duration {
    if retry_after > Duration::ZERO {
        if retry_after > CFPROXY_429_MAX_COOLDOWN {
            return CFPROXY_429_MAX_COOLDOWN;
        }
        return retry_after;
    }
    let mut strikes = prev.strikes;
    let expired = match prev.until {
        None => true,
        Some(u) => u.elapsed() > CFPROXY_429_MAX_COOLDOWN,
    };
    if expired {
        strikes = 0;
    }
    let mut delay = CFPROXY_429_COOLDOWN;
    for _ in 0..strikes {
        delay *= 2;
        if delay >= CFPROXY_429_MAX_COOLDOWN {
            return CFPROXY_429_MAX_COOLDOWN;
        }
    }
    if delay > CFPROXY_429_MAX_COOLDOWN {
        return CFPROXY_429_MAX_COOLDOWN;
    }
    delay
}

pub fn mark_cfproxy_429_cooldown(domain: &str, err: &WsError) {
    let d = normalize_cf_domain(domain);
    if d.is_empty() {
        return;
    }
    let retry_after = retry_after_delay(err);
    let mut map = CFPROXY_429.write();
    let prev = map.get(&d).cloned().unwrap_or_default();
    let delay = next_cfproxy_429_cooldown_delay(&prev, retry_after);
    let mut strikes = prev.strikes + 1;
    let expired = match prev.until {
        None => true,
        Some(u) => u.elapsed() > CFPROXY_429_MAX_COOLDOWN,
    };
    if expired {
        strikes = 1;
    }
    map.insert(
        d.clone(),
        Cfproxy429State {
            until: Some(Instant::now() + delay),
            strikes,
        },
    );
    drop(map);
    ldebug!(" CF cooldown {}: {:.0}s after 429", d, delay.as_secs_f64().ceil());
}

pub fn cfproxy_429_cooldown_remaining(domain: &str) -> Duration {
    let d = normalize_cf_domain(domain);
    if d.is_empty() {
        return Duration::ZERO;
    }
    let map = CFPROXY_429.read();
    let state = match map.get(&d) {
        Some(s) => s.clone(),
        None => return Duration::ZERO,
    };
    drop(map);
    let until = match state.until {
        Some(u) => u,
        None => return Duration::ZERO,
    };
    let now = Instant::now();
    if until <= now {
        CFPROXY_429.write().remove(&d);
        return Duration::ZERO;
    }
    until - now
}

pub async fn acquire_cfproxy_attempt_slot() -> Option<tokio::sync::SemaphorePermit<'static>> {
    CFPROXY_SEM.acquire().await.ok()
}

// ---------------------------------------------------------------------------
// Cache files
// ---------------------------------------------------------------------------

fn cfproxy_cache_path() -> Option<PathBuf> {
    let dir = CFPROXY.read().cache_dir.trim().to_string();
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir).join(CFPROXY_CACHE_FILE_NAME))
}

// Активный домен больше не сохраняется в отдельный файл. Балансер работает в памяти.

fn load_cfproxy_domains_from_cache() -> Vec<String> {
    let path = match cfproxy_cache_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let list: Vec<String> = data.split('\n').map(|s| s.to_string()).collect();
    merge_cfproxy_domains(&[list])
}



fn save_cfproxy_domains_to_cache(domains: &[String]) {
    let path = match cfproxy_cache_path() {
        Some(p) => p,
        None => return,
    };
    if domains.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            ldebug!(" CF: кеш создать не удалось: {}", e);
            return;
        }
    }
    let data = domains.join("\n");
    if let Err(e) = std::fs::write(&path, data) {
        ldebug!(" CF: кеш сохранить не удалось: {}", e);
    }
}



fn should_refresh_cfproxy_domains() -> bool {
    let path = match cfproxy_cache_path() {
        Some(p) => p,
        None => return true,
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => return true,
    };
    let modified = match meta.modified() {
        Ok(t) => t,
        Err(_) => return true,
    };
    match modified.elapsed() {
        Ok(elapsed) => elapsed >= CFPROXY_REFRESH_INTERVAL,
        Err(_) => true,
    }
}



pub fn init_cfproxy_domains() {
    let defaults = default_cfproxy_domains();
    let cached = load_cfproxy_domains_from_cache();

    let mut cfg = CFPROXY.write();
    if !cfg.user_domain.is_empty() {
        let ud = cfg.user_domain.clone();
        cfg.domains = vec![ud.clone()];
        crate::balancer::BALANCER.write().update_domains_list(&cfg.domains);
        return;
    }

    if !cached.is_empty() {
        let n = cached.len();
        cfg.domains = merge_cfproxy_domains(&[cached, defaults]);
        crate::balancer::BALANCER.write().update_domains_list(&cfg.domains);
        drop(cfg);
        linfo!(" CF: кеш доменов загружен ({} шт.)", n);
    } else {
        cfg.domains = defaults;
        crate::balancer::BALANCER.write().update_domains_list(&cfg.domains);
    }
}

pub fn start_cfproxy_refresh() {
    // Порт proxy/config.py::start_cfproxy_domain_refresh: сначала пробуем
    // обновить сразу (до 3 попыток), затем раз в час фоновым таском.
    // На мобиле уважаем свежесть дискового кеша для первой попытки.
    if !should_refresh_cfproxy_domains() {
        ldebug!(" CF: кеш свежий, откладываю обновление списка на час");
    }
    tokio::spawn(async move {
        if should_refresh_cfproxy_domains() {
            for _ in 0..3 {
                if try_refresh_cfproxy_domains().await {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            if CFPROXY.read().domains.is_empty() {
                ldebug!(" CF: обновить список доменов не удалось, остаюсь на кеше/встроенном списке");
            }
        }
        loop {
            tokio::time::sleep(CFPROXY_REFRESH_INTERVAL).await;
            try_refresh_cfproxy_domains().await;
        }
    });
}

pub async fn try_refresh_cfproxy_domains() -> bool {
    let has_user = !CFPROXY.read().user_domain.is_empty();
    if has_user {
        return true;
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Cache-buster как в оригинале ("?" + 7 случайных букв), UA tg-ws-proxy.
    let nonce: String = {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..7)
            .map(|_| rng.gen_range(b'a'..=b'z') as char)
            .collect()
    };
    let url = format!("{}?{}", CFPROXY_DOMAINS_URL, nonce);
    let resp = match client
        .get(&url)
        .header("User-Agent", "tg-ws-proxy")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            ldebug!(" CF: GitHub недоступен: {}", e);
            return false;
        }
    };
    if resp.status().as_u16() != 200 {
        ldebug!(" CF: GitHub вернул {}", resp.status().as_u16());
        return false;
    }
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            ldebug!(" CF: список доменов прочитать не удалось: {}", e);
            return false;
        }
    };

    let mut fetched_raw = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        fetched_raw.push(line.to_string());
    }

    // Валидация пула как в оригинале: минимум CFPROXY_MIN_VALID_DOMAINS
    // валидных доменов, иначе игнорируем payload и остаёмся на текущем пуле.
    let decoded: Vec<String> = fetched_raw
        .iter()
        .map(|d| normalize_encoded_cf_domain(d))
        .collect();
    let pool = normalize_domain_pool(&decoded);
    if pool.len() >= CFPROXY_MIN_VALID_DOMAINS {
        let merged = merge_cfproxy_domains(&[pool.clone(), default_cfproxy_domains()]);
        {
            let mut cfg = CFPROXY.write();
            if !cfg.user_domain.is_empty() {
                return true;
            }
            cfg.domains = merged.clone();
        }
        crate::balancer::BALANCER.write().update_domains_list(&merged);
        save_cfproxy_domains_to_cache(&merged);
        linfo!(" CF: список доменов обновлен ({} шт.)", pool.len());
        return true;
    }

    if !fetched_raw.is_empty() {
        lwarn!(
            " CF: игнорирую список доменов (total={}, valid={}, need>={}); остаюсь на текущем пуле",
            fetched_raw.len(),
            pool.len(),
            CFPROXY_MIN_VALID_DOMAINS
        );
    } else {
        ldebug!(" CF: обновить список доменов не удалось (пустой ответ), остаюсь на кеше/встроенном списке");
    }
    false
}

// ---------------------------------------------------------------------------
// DNS over HTTPS (DoH) resolve (с поддержкой мобильных сетей РФ и IPv6)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DohAnswer {
    #[serde(rename = "data")]
    data: String,
    #[serde(rename = "type")]
    type_: i32,
}
#[derive(Deserialize)]
struct DohResponse {
    #[serde(rename = "Answer", default)]
    answer: Vec<DohAnswer>,
}

static DOH_CACHE: Lazy<parking_lot::RwLock<std::collections::HashMap<String, (String, Instant)>>> =
    Lazy::new(|| parking_lot::RwLock::new(std::collections::HashMap::new()));

/// Последний успешно разрешённый IP. НЕ очищается при смене сети: сразу после
/// Wi-Fi ↔ LTE системный резолвер часто отдаёт "nodename nor servname provided",
/// и полная очистка кеша превращала это в лавину отказов по всем 20 доменам.
static DOH_STICKY: Lazy<parking_lot::RwLock<std::collections::HashMap<String, String>>> =
    Lazy::new(|| parking_lot::RwLock::new(std::collections::HashMap::new()));
static DOH_LAST_FAILURE: Lazy<parking_lot::RwLock<std::collections::HashMap<String, Instant>>> =
    Lazy::new(|| parking_lot::RwLock::new(std::collections::HashMap::new()));
static NETWORK_CHANGED_AT: Lazy<parking_lot::RwLock<Option<Instant>>> =
    Lazy::new(|| parking_lot::RwLock::new(None));

const NETWORK_GRACE: Duration = Duration::from_secs(3);
const DOH_FAILURE_BACKOFF: Duration = Duration::from_secs(2);

/// Раньше здесь был `DOH_CACHE.write().clear()`. Теперь только помечаем
/// короткий grace-период, чтобы не долбить резолвер в момент переключения.
pub fn clear_doh_cache() {
    *NETWORK_CHANGED_AT.write() = Some(Instant::now());
}

pub fn network_grace_active() -> bool {
    match *NETWORK_CHANGED_AT.read() {
        Some(at) => Instant::now().saturating_duration_since(at) < NETWORK_GRACE,
        None => false,
    }
}

pub fn doh_sticky_ip(domain: &str) -> Option<String> {
    DOH_STICKY.read().get(domain).cloned()
}

/// Полный сброс вызывается только при явном Start/Stop прокси, не при смене сети.
pub fn reset_network_state() {
    *CF_FAST_ROUTE.write() = None;
    CF_BAD_ROUTES.write().clear();
    PORT80_SHORT_DROPS.store(0, std::sync::atomic::Ordering::Relaxed);
    *PORT80_PENALTY_UNTIL.write() = None;
    DOH_CACHE.write().clear();
    DOH_STICKY.write().clear();
    DOH_LAST_FAILURE.write().clear();
    *NETWORK_CHANGED_AT.write() = None;
    *CF_FAILURE_WINDOW.write() = (Instant::now(), 0);
    *CF_COOLDOWN_UNTIL.write() = None;
    CF_LOG_THROTTLE.write().clear();
}

fn pick_preferred_ip(candidates: &[String]) -> String {
    let mut fallback_v6 = String::new();
    for c in candidates {
        let c = c.trim();
        if let Ok(ip) = c.parse::<std::net::IpAddr>() {
            match ip {
                std::net::IpAddr::V4(v4) => return v4.to_string(),
                std::net::IpAddr::V6(v6) => {
                    if fallback_v6.is_empty() {
                        fallback_v6 = v6.to_string();
                    }
                }
            }
        }
    }
    fallback_v6
}

pub async fn resolve_doh(domain: &str) -> Option<String> {
    if let Some((ip, exp)) = DOH_CACHE.read().get(domain).cloned() {
        if Instant::now() < exp {
            return Some(ip);
        }
    }

    let sticky = doh_sticky_ip(domain);
    let recently_failed = DOH_LAST_FAILURE
        .read()
        .get(domain)
        .map(|at| Instant::now().saturating_duration_since(*at) < DOH_FAILURE_BACKOFF)
        .unwrap_or(false);

    // Во время переключения сети или сразу после DNS-отказа не дёргаем
    // резолвер повторно: работаем по последнему известному IP.
    if sticky.is_some() && (network_grace_active() || recently_failed) {
        return sticky.clone();
    }

    // Включаем Яндекс DoH (работает во всех мобильных сетях РФ и белых списках)
    // и прямое обращение к 8.8.8.8 по HTTPS без зависимости от системного DNS.
    let endpoints = [
        "https://common.dns.yandex.net/dns-query",
        "https://8.8.8.8/dns-query",
        "https://dns.google/dns-query",
        "https://dns.adguard-dns.com/dns-query",
        "https://cloudflare-dns.com/dns-query",
    ];

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            DOH_LAST_FAILURE
                .write()
                .insert(domain.to_string(), Instant::now());
            return sticky;
        }
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(endpoints.len() + 1);
    let mut tasks = Vec::new();

    for u in endpoints {
        let client = client.clone();
        let domain = domain.to_string();
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            let full = format!("{}?name={}&type=A", u, domain);
            if let Ok(resp) = client
                .get(&full)
                .header("Accept", "application/dns-json")
                .send()
                .await
            {
                if resp.status().as_u16() == 200 {
                    if let Ok(r) = resp.json::<DohResponse>().await {
                        for ans in r.answer {
                            if (ans.type_ == 1 || ans.type_ == 28)
                                && ans.data.trim().parse::<std::net::IpAddr>().is_ok()
                            {
                                let _ = tx.send(Some(ans.data.trim().to_string())).await;
                                return;
                            }
                        }
                    }
                }
            }
            let _ = tx.send(None).await;
        }));
    }

    // UDP-резолв через системный resolver как дополнительный кандидат
    {
        let domain2 = domain.to_string();
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            let host = format!("{}:443", domain2);
            if let Ok(Ok(addrs)) = tokio::time::timeout(
                Duration::from_millis(1500),
                tokio::net::lookup_host(host),
            )
            .await
            {
                let ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
                let p = pick_preferred_ip(&ips);
                if !p.is_empty() {
                    let _ = tx.send(Some(p)).await;
                    return;
                }
            }
            let _ = tx.send(None).await;
        }));
    }

    drop(tx); // Чтобы rx.recv() завершился, когда все таски завершатся

    let deadline = tokio::time::sleep(Duration::from_millis(1500));
    tokio::pin!(deadline);

    let mut final_ip = None;
    loop {
        tokio::select! {
            _ = &mut deadline => {
                break;
            }
            msg = rx.recv() => {
                match msg {
                    Some(Some(ip)) => {
                        final_ip = Some(ip);
                        break;
                    }
                    Some(None) => {} // Таска ничего не нашла
                    None => break,   // Все таски завершились
                }
            }
        }
    }

    // Отменяем все незавершенные фоновые таски (исправление утечки)
    for t in tasks {
        t.abort();
    }

    if let Some(ip) = &final_ip {
        DOH_CACHE.write().insert(
            domain.to_string(),
            (ip.clone(), Instant::now() + Duration::from_secs(300)),
        );
        DOH_STICKY.write().insert(domain.to_string(), ip.clone());
        DOH_LAST_FAILURE.write().remove(domain);
    } else {
        DOH_LAST_FAILURE
            .write()
            .insert(domain.to_string(), Instant::now());
        // Ошибка резолвера != домен недоступен. Возвращаем sticky IP,
        // чтобы `ws_connect_once_full` не превращал это в "no fallback".
        if let Some(ip) = sticky {
            return Some(ip);
        }
    }

    final_ip
}

// ---------------------------------------------------------------------------
// cfConnectDomain — с мультипортовым фоллбэком и чистыми Cloudflare Anycast IP
// ---------------------------------------------------------------------------

static CLEAN_IP_ROUND_ROBIN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn new_timed_attempt_timeout(base: Duration, phase: Duration) -> Duration {
    let mut eff = base;
    if eff <= Duration::ZERO {
        eff = Duration::from_secs(5);
    }
    if eff > phase {
        eff = phase;
    }
    eff
}

/// Параллельная Happy-Eyeballs гонка для мобильных сетей:
/// одновременно проверяет чистые Anycast IP из 3 независимых подсетей (188.114.96.0/22,
/// 162.159.135.0/22, 172.64.148.0/20) на обоих портах (80 HTTP и 443 TLS с разделением ClientHello),
/// а также штатный хостнейм, не тратя время на ожидание таймаута заблокированных 104.21/172.67 и DNS.
async fn race_cf_mobile_endpoints(
    domain: &str,
    path: &str,
    attempt_timeout: Duration,
) -> (Option<RawWebSocket>, String, Option<WsError>) {
    // (dial_addr, is_clean_ip, port, secure)
    let mut candidates: Vec<(String, bool, u16, bool)> = Vec::with_capacity(8);

    // Если оператор режет :80 после 101, принудительно уходим на TLS 443,
    // даже если пользователь включил "Порт 80 для Cloudflare (No TLS)".
    let prefer_plain = DISABLE_SECURE.load(std::sync::atomic::Ordering::Relaxed)
        && !port80_penalty_active();
    let total_clean = CF_CLEAN_IPS.len();
    if total_clean >= 3 {
        let base_idx =
            (CLEAN_IP_ROUND_ROBIN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) * 3)
                % total_clean;
        let ip1 = CF_CLEAN_IPS[base_idx].to_string(); // 188.114.96..99.3
        let ip2 = CF_CLEAN_IPS[(base_idx + 1) % total_clean].to_string(); // 162.159.135..138.42
        let ip3 = CF_CLEAN_IPS[(base_idx + 2) % total_clean].to_string(); // 172.64.148..155.x

        for ip in [ip1, ip2, ip3] {
            let ports = if prefer_plain && !is_cf_route_bad(&ip, 80) {
                [(80u16, false), (443u16, true)]
            } else {
                [(443u16, true), (80u16, false)]
            };
            for (p, sec) in ports {
                if !is_cf_route_bad(&ip, p) {
                    candidates.push((ip.clone(), true, p, sec));
                }
            }
        }
    }

    // Если все кандидаты были в штрафном списке — снимаем штраф ТОЛЬКО с
    // конкретных IP на 443. Раньше здесь был `CF_BAD_ROUTES.write().clear()`,
    // который мгновенно отменял ("*", 80) и возвращал ядро на режущийся порт.
    if candidates.is_empty() && total_clean >= 2 {
        let ip1 = CF_CLEAN_IPS[0].to_string();
        let ip2 = CF_CLEAN_IPS[1].to_string();
        {
            let mut bad = CF_BAD_ROUTES.write();
            bad.remove(&(ip1.clone(), 443));
            bad.remove(&(ip2.clone(), 443));
            if !port80_penalty_active() {
                bad.remove(&(ip1.clone(), 80));
                bad.remove(&(ip2.clone(), 80));
            }
        }
        candidates.push((ip1.clone(), true, 443, true));
        candidates.push((ip2.clone(), true, 443, true));
        if !port80_penalty_active() {
            candidates.push((ip1, true, 80, false));
            candidates.push((ip2, true, 80, false));
        }
    }

    // Доменный кандидат требует системного DNS. В grace-период после смены
    // сети он почти всегда даёт "nodename nor servname provided" — не добавляем.
    if !network_grace_active() {
        let def_port = if prefer_plain && !is_cf_route_bad(domain, 80) { 80 } else { 443 };
        candidates.push((domain.to_string(), false, def_port, def_port == 443));
    }

    let race_timeout = attempt_timeout.clamp(Duration::from_millis(1500), Duration::from_millis(2500));
    // В grace-период доменный кандидат не добавляется: channel(0) паниковал бы.
    let (tx, mut rx) = tokio::sync::mpsc::channel(candidates.len().max(1));
    let mut tasks = Vec::with_capacity(candidates.len());

    for (idx, (dial_addr, is_clean, port, secure)) in candidates.into_iter().enumerate() {
        let domain_s = domain.to_string();
        let path_s = path.to_string();
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            // Небольшой ступенчатый сдвиг (0ms для первой пары, 35ms для остальных)
            if idx >= 2 {
                tokio::time::sleep(Duration::from_millis((idx as u64) * 30)).await;
            }
            match ws_connect_once_port(&dial_addr, port, &domain_s, &path_s, race_timeout, None, secure)
                .await
            {
                Ok(ws) => {
                    let route = CfFastRoute {
                        clean_ip: if is_clean {
                            Some(dial_addr.clone())
                        } else {
                            None
                        },
                        port,
                        secure,
                        expires_at: Instant::now() + Duration::from_secs(600),
                    };
                    let _ = tx.send(Ok((ws, dial_addr, route))).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                }
            }
        }));
    }
    drop(tx);

    let deadline = tokio::time::sleep(race_timeout + Duration::from_millis(150));
    tokio::pin!(deadline);

    let mut last_err: Option<WsError> = None;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            msg = rx.recv() => {
                match msg {
                    Some(Ok((ws, winning_ip, winning_route))) => {
                        for t in tasks {
                            t.abort();
                        }
                        linfo!(
                            " CF mobile route connected: {} via {}:{} (tls={})",
                            domain,
                            winning_ip,
                            winning_route.port,
                            winning_route.secure
                        );
                        // НЕ сохраняем маршрут в CF_FAST_ROUTE до проверки реального трафика в bridge_ws,
                        // чтобы сброс ТСПУ после 101 не зацикливал прокси на заблокированном порту!
                        return (Some(ws), winning_ip, None);
                    }
                    Some(Err(e)) => {
                        if is_http_status_error(&e, 429) {
                            for t in tasks {
                                t.abort();
                            }
                            return (None, String::new(), Some(e));
                        }
                        last_err = Some(e);
                    }
                    None => break,
                }
            }
        }
    }

    for t in tasks {
        t.abort();
    }

    // Если чистые IP не подошли (например, пользовательский домен не на Cloudflare), пробуем DoH IP.
    // Явно задаём порт/TLS: ws_connect_once() использовал бы DISABLE_SECURE и увёл бы
    // соединение на :80 даже во время действующего штрафа порта 80.
    if let Some(resolved_ip) = resolve_doh(domain).await {
        if !resolved_ip.is_empty() {
            let ip_timeout = attempt_timeout.min(Duration::from_millis(1800));
            let (ip_port, ip_secure) = if prefer_plain {
                (80u16, false)
            } else {
                (443u16, true)
            };
            if let Ok(ws) = ws_connect_once_port(
                &resolved_ip,
                ip_port,
                domain,
                path,
                ip_timeout,
                None,
                ip_secure,
            )
            .await
            {
                return (Some(ws), resolved_ip, None);
            }
        }
    }

    (None, String::new(), Some(last_err.unwrap_or(WsError::Timeout)))
}

pub async fn cf_connect_domain(
    domain: &str,
    path: &str,
    timeout: f64,
) -> (Option<RawWebSocket>, String, Option<WsError>) {
    let path = if path.is_empty() { "/apiws" } else { path };

    let attempt_timeout = crate::ws::ws_connect_timeout(timeout);
    let mut phase_timeout = attempt_timeout;
    if phase_timeout > CFPROXY_DIAL_PHASE_TIMEOUT {
        phase_timeout = CFPROXY_DIAL_PHASE_TIMEOUT;
    }

    // 1. Быстрый путь по закешированному рабочему маршруту (чистый Anycast IP + рабочий порт 80/443)
    let cached_route = {
        let guard = CF_FAST_ROUTE.read();
        guard.as_ref().and_then(|r| {
            if Instant::now() < r.expires_at && !(r.port == 80 && port80_penalty_active()) {
                Some(r.clone())
            } else {
                None
            }
        })
    };

    if let Some(route) = cached_route {
        let fast_timeout = new_timed_attempt_timeout(phase_timeout, Duration::from_millis(1500));
        let dial_target = route.clean_ip.as_deref().unwrap_or(domain);
        match ws_connect_once_port(
            dial_target,
            route.port,
            domain,
            path,
            fast_timeout,
            None,
            route.secure,
        )
        .await
        {
            Ok(ws) => {
                return (
                    Some(ws),
                    route.clean_ip.unwrap_or_default(),
                    None,
                );
            }
            Err(e) => {
                if is_http_status_error(&e, 429) {
                    return (None, String::new(), Some(e));
                }
                // Кешированный маршрут перестал отвечать — сбрасываем и запускаем параллельную гонку
                clear_cf_fast_route();
            }
        }
    }

    // 2. Параллельная Happy-Eyeballs гонка по чистым Anycast IP (188.114.x.x, 162.159.x.x, 172.64.x.x)
    // на портах 80 и 443 без ожидания таймаута заблокированных 104.21/172.67 и мобильного DNS.
    race_cf_mobile_endpoints(domain, path, phase_timeout).await
}

pub fn log_cf_conn_error(msg: &str, err: &WsError) {
    if !should_log_cf_error(msg) {
        return;
    }
    if let WsError::Io(e) = err {
        if e.kind() == std::io::ErrorKind::ConnectionReset {
            return;
        }
    }
    if is_http_status_error(err, 429) {
        lwarn!("{}", msg);
    } else {
        lerror!("{}", msg);
    }
}

// активный домен set/save
pub fn set_active_domain_and_save(_chosen: &str) {
    // Больше не используется для файлов. Балансер обновляется внутри proxy.rs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_cf_domain_builtin_and_custom() {
        // Встроенный кодированный домен должен декодироваться в .co.uk
        let builtin = normalize_cf_domain(CFPROXY_ENC[0]);
        assert!(builtin.ends_with(".co.uk"), "got {}", builtin);

        // Любой пользовательский домен (.com, .ru, .workers.dev) должен сохраняться как есть
        assert_eq!(normalize_cf_domain("my-proxy.example.com"), "my-proxy.example.com");
        assert_eq!(normalize_cf_domain("tg.my-domain.ru"), "tg.my-domain.ru");
        assert_eq!(
            normalize_cf_domain("https://tg-ws.user.workers.dev/apiws"),
            "tg-ws.user.workers.dev"
        );
        assert!(is_worker_style_domain("tg-ws.user.workers.dev"));
        assert!(is_worker_style_domain("my-app.pages.dev"));
        assert!(!is_worker_style_domain("my-domain.ru"));
    }

    /// Один последовательный тест на всё разделяемое состояние: параллельные
    /// тесты могли бы перезаписывать статики друг друга.
    #[test]
    fn test_network_state_survives_network_change() {
        reset_network_state();

        // Два сброса после 101 включают 5-минутный запрет порта 80.
        mark_cf_route_unstable("188.114.98.3", 80);
        assert!(!port80_penalty_active());
        mark_cf_route_unstable("188.114.98.3", 80);
        assert!(port80_penalty_active());
        assert!(is_cf_route_bad("188.114.98.3", 80));
        assert!(!is_cf_route_bad("188.114.98.3", 443));

        // Смена сети больше не отменяет штраф и не выжигает sticky DNS.
        clear_cf_fast_route();
        clear_doh_cache();
        assert!(port80_penalty_active());
        assert!(network_grace_active());

        DOH_STICKY
            .write()
            .insert("kws2.example.co.uk".to_string(), "188.114.98.3".to_string());
        clear_doh_cache();
        assert_eq!(
            doh_sticky_ip("kws2.example.co.uk"),
            Some("188.114.98.3".to_string())
        );

        // Шторм отказов включает паузу, первый же успех её снимает.
        for _ in 0..CF_STORM_THRESHOLD {
            note_cf_attempt_failure();
        }
        assert!(cf_network_cooldown_active());
        note_cf_attempt_success();
        assert!(!cf_network_cooldown_active());

        // Одинаковые `CF fail ...` не должны заполнять логи сотнями строк.
        assert!(should_log_cf_error(" CF fail kws2.example.co.uk: timeout"));
        assert!(!should_log_cf_error(" CF fail kws2.example.co.uk: timeout"));

        // Явный Start/Stop прокси сбрасывает состояние полностью.
        reset_network_state();
        assert!(!port80_penalty_active());
        assert!(!network_grace_active());
        assert!(doh_sticky_ip("kws2.example.co.uk").is_none());
    }
}
