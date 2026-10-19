use crate::config::*;
use crate::ws::{is_http_status_error, ws_connect_once, ws_connect_once_port, RawWebSocket, WsError};
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

pub fn clear_cf_fast_route() {
    *CF_FAST_ROUTE.write() = None;
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

pub fn clear_doh_cache() {
    DOH_CACHE.write().clear();
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

    // Включаем Яндекс DoH (работает во всех мобильных сетях РФ и белых списках)
    // и прямое обращение к 8.8.8.8 по HTTPS без зависимости от системного DNS.
    let endpoints = [
        "https://common.dns.yandex.net/dns-query",
        "https://8.8.8.8/dns-query",
        "https://dns.google/dns-query",
        "https://dns.adguard-dns.com/dns-query",
        "https://cloudflare-dns.com/dns-query",
    ];

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .ok()?;

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

    let prefer_plain = DISABLE_SECURE.load(std::sync::atomic::Ordering::Relaxed);
    let total_clean = CF_CLEAN_IPS.len();
    if total_clean >= 3 {
        let base_idx =
            (CLEAN_IP_ROUND_ROBIN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) * 3)
                % total_clean;
        let ip1 = CF_CLEAN_IPS[base_idx].to_string(); // 188.114.96..99.3
        let ip2 = CF_CLEAN_IPS[(base_idx + 1) % total_clean].to_string(); // 162.159.135..138.42
        let ip3 = CF_CLEAN_IPS[(base_idx + 2) % total_clean].to_string(); // 172.64.148..155.x

        for ip in [ip1, ip2, ip3] {
            if prefer_plain {
                candidates.push((ip.clone(), true, 80, false));
                candidates.push((ip, true, 443, true));
            } else {
                candidates.push((ip.clone(), true, 443, true));
                candidates.push((ip, true, 80, false));
            }
        }
    }

    // Также параллельно запускаем подключение по самому домену (на случай кастомного не-CF хоста)
    let def_port = if prefer_plain { 80 } else { 443 };
    candidates.push((domain.to_string(), false, def_port, !prefer_plain));

    let race_timeout = attempt_timeout.clamp(Duration::from_millis(1500), Duration::from_millis(2500));
    let (tx, mut rx) = tokio::sync::mpsc::channel(candidates.len());
    let mut tasks = Vec::with_capacity(candidates.len());

    for (idx, (dial_addr, is_clean, port, secure)) in candidates.into_iter().enumerate() {
        let domain_s = domain.to_string();
        let path_s = path.to_string();
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            // Небольшой ступенчатый сдвиг (0ms для первой пары, 40ms для остальных), чтобы не дублировать нагрузку
            if idx >= 2 {
                tokio::time::sleep(Duration::from_millis((idx as u64) * 25)).await;
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
                            " CF mobile route ok: {} via {}:{} (tls={})",
                            domain,
                            winning_ip,
                            winning_route.port,
                            winning_route.secure
                        );
                        *CF_FAST_ROUTE.write() = Some(winning_route);
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

    // Если чистые IP не подошли (например, пользовательский домен не на Cloudflare), пробуем DoH IP
    if let Some(resolved_ip) = resolve_doh(domain).await {
        if !resolved_ip.is_empty() {
            let ip_timeout = attempt_timeout.min(Duration::from_millis(1800));
            if let Ok(ws) = ws_connect_once(&resolved_ip, domain, path, ip_timeout).await {
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
            if Instant::now() < r.expires_at {
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
}